//! Cancellable offline decoding. This path never constructs an output device.

use super::{
    PlaybackRange, SourceIdentity,
    source::{DECODE_WORK_BUDGET, DecodeStep, Source, check_cancelled},
};
use crate::DecoderHint;
use anyhow::{Context, Result, bail};
use rivu_format::input::open_regular_file;
use std::{fs::OpenOptions, io::BufWriter, path::Path, sync::atomic::AtomicBool};

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

/// Render the selected source range as native-rate, unclipped float32 WAV.
/// The caller owns the destination directory and must publish only a successful
/// result. Existing destinations are never overwritten; errors remove only the
/// new, partial file. PCM and buffered writes remain bounded independently of
/// track duration, and cancellation reaches decoder packet loops as well.
pub fn render_source(
    path: &Path,
    range: Option<PlaybackRange>,
    decoder_hint: DecoderHint,
    media_read_buffer_mb: u32,
    ffmpeg_enabled: bool,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<RenderInfo> {
    check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
    let buffer_len = usize::try_from(media_read_buffer_mb)
        .context("Audio render media buffer size does not fit usize")?
        .checked_mul(super::BYTES_PER_MEBIBYTE)
        .context("Audio render media buffer size overflows")?;
    let file = open_regular_file(path)?;
    let identity_file = file
        .try_clone()
        .context("Retaining audio render source identity")?;
    let mut source = Source::open_file_with_cancel(
        path,
        file,
        buffer_len,
        decoder_hint,
        ffmpeg_enabled,
        Some(cancelled),
    )?;
    if let Some(range) = range {
        source.restrict_with_cancel(range, Some(cancelled))?;
    }
    let sample_rate = source.info().sample_rate;
    let channels =
        u16::try_from(source.layout().len()).context("Audio render channel count overflows")?;
    if channels == 0 || sample_rate == 0 {
        bail!("Audio render requires a nonempty, fixed-rate channel layout");
    }
    let source_identity = source.identity();
    check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
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
                DecodeStep::Frames((samples, _)) => samples,
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
        std::fs::remove_file(destination)
            .with_context(|| format!("Removing partial audio render {}", destination.display()))?;
    }
    result
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
        let info = render_source(
            &source,
            Some(PlaybackRange {
                start_seconds: 0.025,
                end_seconds: Some(0.075),
            }),
            DecoderHint::Native,
            2,
            false,
            &output,
            &AtomicBool::new(false),
        )
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
    fn cancellation_and_existing_destination_never_modify_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = float_source(directory.path());
        let output = directory.path().join("output.wav");
        let cancelled = AtomicBool::new(true);
        assert!(
            render_source(
                &source,
                None,
                DecoderHint::Native,
                2,
                false,
                &output,
                &cancelled
            )
            .is_err()
        );
        assert!(!output.exists());
        cancelled.store(false, Ordering::Relaxed);
        std::fs::write(&output, b"keep").unwrap();
        assert!(
            render_source(
                &source,
                None,
                DecoderHint::Native,
                2,
                false,
                &output,
                &cancelled
            )
            .is_err()
        );
        assert_eq!(std::fs::read(output).unwrap(), b"keep");
    }
}
