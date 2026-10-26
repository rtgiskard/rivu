//! Cancellable offline decoding. This path never constructs an output device.

use super::{
    PlaybackRange, SourceIdentity,
    source::{DECODE_WORK_BUDGET, DecodeStep, Source, SourceSelection, check_cancelled},
};
use anyhow::{Context, Result, bail};
use rivu_format::input::open_regular_file;
use rivu_format::media::DecoderHint;
use std::{
    fs::{File, OpenOptions},
    io::BufWriter,
    path::Path,
    sync::atomic::AtomicBool,
};

// RIFF uses 32-bit lengths. Leave room for all WAV headers; reject rather than
// wrap a long job's lengths or silently truncate its source timeline.
const MAX_RENDER_BYTES: u64 = u32::MAX as u64 - 1024;
const WRITE_FRAMES: usize = 4096;

#[derive(Clone, Debug)]
pub struct RenderInfo {
    pub source_identity: SourceIdentity,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

/// Format and duration from the actual decoder retained for rendering.
#[derive(Clone, Debug)]
pub struct RenderFormat {
    pub source_identity: SourceIdentity,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: Option<f64>,
}

impl RenderFormat {
    /// Admission estimate only; decoding still enforces its actual byte limit.
    pub fn estimated_frames(&self) -> Option<u64> {
        self.duration
            .filter(|duration| duration.is_finite() && *duration >= 0.0)
            .map(|duration| (duration * f64::from(self.sample_rate)).ceil() as u64)
    }
}

/// Owns one opened decoder, so admission and rendering inspect the same source.
pub struct SourceRenderer {
    source: Source,
    identity_file: File,
    format: RenderFormat,
}

impl SourceRenderer {
    pub fn open(
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        media_read_buffer_mb: u32,
        ffmpeg_enabled: bool,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
        let buffer_len = usize::try_from(media_read_buffer_mb)
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
            channels,
            duration: source.info().duration,
        };
        check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
        Ok(Self {
            source,
            identity_file,
            format,
        })
    }

    pub fn format(&self) -> &RenderFormat {
        &self.format
    }

    /// Native-rate float32 WAV, bounded by the caller's whole-file byte allowance.
    /// Existing destinations are never overwritten; failures remove the partial file.
    pub fn render(
        self,
        destination: &Path,
        max_bytes: u64,
        cancelled: &AtomicBool,
    ) -> Result<RenderInfo> {
        check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
        let Self {
            mut source,
            identity_file,
            format,
        } = self;
        if format.estimated_frames().is_some_and(|frames| {
            frames
                .checked_mul(u64::from(format.channels) * 4)
                .and_then(|bytes| bytes.checked_add(128))
                .is_none_or(|bytes| bytes > max_bytes)
        }) {
            bail!("Audio render exceeds task byte limit");
        }
        if format.estimated_frames().is_some_and(|frames| {
            frames
                .checked_mul(u64::from(format.channels) * 4)
                .is_none_or(|bytes| bytes > MAX_RENDER_BYTES)
        }) {
            bail!("Audio render exceeds the float WAV RIFF size limit");
        }
        let RenderFormat {
            source_identity,
            sample_rate,
            channels,
            ..
        } = format;
        if max_bytes < 128 {
            bail!("Audio render exceeds task byte limit");
        }
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .with_context(|| format!("Creating audio render {}", destination.display()))?;
        let result = (|| {
            let mut writer = hound::WavWriter::new(
                BufWriter::with_capacity(64 * 1024, output),
                hound::WavSpec {
                    channels,
                    sample_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                },
            )?;
            let mut frames = 0_u64;
            loop {
                let mut budget = DECODE_WORK_BUDGET;
                let samples = match source.next_frames_with_cancel(Some(cancelled), &mut budget)? {
                    DecodeStep::Frames(span) => span.samples,
                    DecodeStep::Yield => continue,
                    DecodeStep::Eof => break,
                };
                if !samples.len().is_multiple_of(usize::from(channels)) {
                    bail!("Audio render ends in an incomplete interleaved frame");
                }
                let next_frames = frames
                    .checked_add((samples.len() / usize::from(channels)) as u64)
                    .context("Audio render frame count overflows")?;
                if next_frames
                    .checked_mul(u64::from(channels) * 4)
                    .is_none_or(|bytes| bytes > MAX_RENDER_BYTES)
                {
                    bail!("Audio render exceeds the float WAV RIFF size limit");
                }
                if next_frames
                    .checked_mul(u64::from(channels) * 4)
                    .and_then(|bytes| bytes.checked_add(128))
                    .is_none_or(|bytes| bytes > max_bytes)
                {
                    bail!("Audio render exceeds task byte limit");
                }
                for chunk in samples.chunks(WRITE_FRAMES * usize::from(channels)) {
                    check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
                    for &sample in chunk {
                        writer.write_sample(sample)?;
                    }
                }
                frames = next_frames;
                if source.layout().len() != usize::from(channels)
                    || source.info().sample_rate != sample_rate
                {
                    bail!("Audio render source format changed within the track");
                }
            }
            check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
            if SourceIdentity::from_file(&identity_file)? != source_identity {
                bail!("Audio render source changed while decoding");
            }
            writer.finalize().context("Finalizing audio render WAV")?;
            check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
            Ok(RenderInfo {
                source_identity,
                sample_rate,
                channels,
                frames,
            })
        })();
        if result.is_err() {
            std::fs::remove_file(destination).with_context(|| {
                format!("Removing partial audio render {}", destination.display())
            })?;
        }
        result
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
    fn render_preserves_selected_stereo_timeline_and_float_overs() {
        let directory = tempfile::tempdir().unwrap();
        let source = float_source(directory.path());
        let output = directory.path().join("selected.wav");
        let info = SourceRenderer::open(
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
        .render(&output, u64::MAX, &AtomicBool::new(false))
        .unwrap();
        assert_eq!(
            (info.sample_rate, info.channels, info.frames),
            (8_000, 2, 400)
        );
        let reader = hound::WavReader::open(output).unwrap();
        assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
        let samples: Vec<f32> = reader
            .into_samples()
            .map(|sample| sample.unwrap())
            .collect();
        for (offset, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(
                pair,
                &[
                    (offset + 200) as f32 / 100.0,
                    -((offset + 200) as f32) / 200.0
                ]
            );
        }
    }

    #[test]
    fn render_budget_rejects_before_creation_and_cleans_unknown_duration_partial() {
        let directory = tempfile::tempdir().unwrap();
        let source = float_source(directory.path());
        let output = directory.path().join("limited.wav");
        let cancelled = AtomicBool::new(false);
        let renderer =
            SourceRenderer::open(&source, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        assert_eq!(renderer.format().channels, 2);
        assert_eq!(renderer.format().sample_rate, 8000);
        assert!(!output.exists());
        assert!(
            renderer
                .render(&output, 256, &cancelled)
                .unwrap_err()
                .to_string()
                .contains("task byte limit")
        );
        assert!(!output.exists());

        let longer = directory.path().join("longer.wav");
        let mut writer = hound::WavWriter::create(
            &longer,
            hound::WavSpec {
                channels: 2,
                sample_rate: 8000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for _ in 0..40_000 {
            writer.write_sample(0.25_f32).unwrap();
        }
        writer.finalize().unwrap();
        let mut renderer =
            SourceRenderer::open(&longer, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        renderer.format.duration = None;
        assert!(
            renderer
                .render(&output, 10_000 * 2 * 4 + 128, &cancelled)
                .unwrap_err()
                .to_string()
                .contains("task byte limit")
        );
        assert!(!output.exists());

        let renderer =
            SourceRenderer::open(&source, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        assert_eq!(
            renderer
                .render(&output, 800 * 2 * 4 + 128, &cancelled)
                .unwrap()
                .frames,
            800
        );
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
        let output = directory.path().join("rendered.wav");
        let info = renderer.render(&output, u64::MAX, &cancelled).unwrap();
        assert_eq!(info.source_identity, identity);
        assert_eq!(info.frames, 800);
        assert_ne!(
            SourceIdentity::from_file(&File::open(source).unwrap()).unwrap(),
            identity
        );
    }

    #[test]
    fn cancellation_and_existing_destination_never_modify_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = float_source(directory.path());
        let output = directory.path().join("output.wav");
        let cancelled = AtomicBool::new(true);
        assert!(
            SourceRenderer::open(&source, None, DecoderHint::Native, 2, false, &cancelled).is_err()
        );
        assert!(!output.exists());
        cancelled.store(false, Ordering::Relaxed);
        std::fs::write(&output, b"keep").unwrap();
        assert!(
            SourceRenderer::open(&source, None, DecoderHint::Native, 2, false, &cancelled)
                .unwrap()
                .render(&output, u64::MAX, &cancelled)
                .is_err()
        );
        assert_eq!(std::fs::read(output).unwrap(), b"keep");
    }
}
