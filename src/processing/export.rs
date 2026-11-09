use super::render::check_cancel;
use anyhow::{Context, Result, bail, ensure};
use rivu_audio as audio;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

pub(super) fn export_destination(destination: &Path) -> Result<PathBuf> {
    ensure!(
        destination.is_absolute(),
        "Export destination must be an absolute path"
    );
    ensure!(
        !destination.try_exists()?,
        "Export destination already exists; choose a new file"
    );
    let parent = fs::canonicalize(
        destination
            .parent()
            .context("Export destination has no parent")?,
    )?;
    ensure!(
        parent.is_dir(),
        "Export destination parent must be a directory"
    );
    Ok(parent.join(
        destination
            .file_name()
            .context("Export destination has no file name")?,
    ))
}

pub(super) fn prepare_export(
    source: &audio::PcmAudio,
    destination: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    prepare_streaming_export(
        source.sample_rate(),
        source.layout(),
        destination,
        cancel,
        |writer| {
            let mut block = vec![0.0; 8192 * usize::from(source.channels())];
            let mut position = 0;
            progress(0, source.frames());
            while position < source.frames() {
                check_cancel(cancel)?;
                let frames = source.read_frames(position, &mut block);
                ensure!(
                    frames > 0,
                    "Export PCM ended before its declared frame count"
                );
                writer.write_frames(&block[..frames * usize::from(source.channels())])?;
                position += frames as u64;
                progress(position, source.frames());
            }
            Ok(())
        },
    )
}

pub(super) struct WavBlockWriter<'a> {
    writer: hound::WavWriter<std::io::BufWriter<&'a mut fs::File>>,
    cancel: &'a AtomicBool,
    order: [usize; 32],
    channels: usize,
    failed: bool,
}

impl WavBlockWriter<'_> {
    pub(super) fn write_frames(&mut self, samples: &[f32]) -> Result<()> {
        ensure!(!self.failed, "Export writer failed a previous block");
        self.failed = true;
        check_cancel(self.cancel)?;
        ensure!(
            samples.len().is_multiple_of(self.channels),
            "Export contains an incomplete interleaved frame"
        );
        // Hound's extensible float32 header is 68 bytes, of which the RIFF
        // length includes 60. Reject before its u32 size accounting can wrap.
        let remaining_samples = (u64::from(u32::MAX) - 60) / 4 - u64::from(self.writer.len());
        ensure!(
            samples.len() as u64 <= remaining_samples,
            "Export exceeds the RIFF/WAVE size limit"
        );
        for block in samples.chunks(8192 * self.channels) {
            check_cancel(self.cancel)?;
            for frame in block.chunks_exact(self.channels) {
                for &index in &self.order[..self.channels] {
                    let sample = frame[index];
                    ensure!(sample.is_finite(), "Export contains a non-finite sample");
                    self.writer.write_sample(sample)?;
                }
            }
        }
        check_cancel(self.cancel)?;
        self.failed = false;
        Ok(())
    }
}

pub(super) fn prepare_streaming_export(
    sample_rate: u32,
    layout: &[audio::Channel],
    destination: &Path,
    cancel: &AtomicBool,
    produce: impl FnOnce(&mut WavBlockWriter<'_>) -> Result<()>,
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    let canonical = export_destination(destination)?;
    ensure!(
        canonical == destination,
        "Export destination parent changed after admission"
    );
    check_cancel(cancel)?;
    ensure!(
        sample_rate > 0 && !layout.is_empty() && layout.len() <= 32,
        "Export requires a nonzero sample rate and a nonempty WAVE speaker layout"
    );
    ensure!(
        sample_rate.checked_mul(layout.len() as u32 * 4).is_some(),
        "Export sample rate exceeds the float32 WAVE byte-rate limit"
    );
    // WAVE masks describe samples in ascending speaker-bit order, unlike an
    // in-memory custom layout, which may retain any decoder-provided ordering.
    let mut order = [0; 32];
    let mut mask = 0_u32;
    for (index, speaker) in layout.iter().enumerate() {
        let bit = 1_u32 << speaker.standard_index();
        ensure!(
            mask & bit == 0,
            "Export has a duplicate speaker: {speaker:?}"
        );
        mask |= bit;
        order[index] = index;
    }
    order[..layout.len()].sort_unstable_by_key(|&index| layout[index].standard_index());
    let spec = hound::WavSpec {
        channels: layout.len() as u16,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut temporary = tempfile::NamedTempFile::new_in(
        canonical
            .parent()
            .context("Export destination has no parent")?,
    )?;
    {
        let mut writer = WavBlockWriter {
            writer: hound::WavWriter::new(std::io::BufWriter::new(temporary.as_file_mut()), spec)?,
            cancel,
            order,
            channels: layout.len(),
            failed: false,
        };
        produce(&mut writer)?;
        ensure!(!writer.failed, "Cannot publish WAV after a failed block");
        check_cancel(cancel)?;
        writer.writer.finalize()?;
    }
    set_wav_channel_mask(temporary.as_file_mut(), spec, mask)?;
    check_cancel(cancel)?;
    Ok((temporary, canonical))
}

/// Hound 3.5 writes float32 as WAVEFORMATEXTENSIBLE, but guesses its mask.
/// Locate and validate the actual RIFF fmt chunk before replacing dwChannelMask
/// on the same unpublished file; hound still owns all encoding and finalization.
fn set_wav_channel_mask(file: &mut fs::File, spec: hound::WavSpec, mask: u32) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    file.rewind()?;
    let length = file.metadata()?.len();
    let mut riff = [0_u8; 12];
    file.read_exact(&mut riff)?;
    ensure!(
        &riff[..4] == b"RIFF"
            && &riff[8..] == b"WAVE"
            && u64::from(u32::from_le_bytes(riff[4..8].try_into()?)) + 8 == length,
        "Export writer produced an invalid RIFF/WAVE container"
    );
    let mut position = 12_u64;
    while position + 8 <= length {
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = [0_u8; 8];
        file.read_exact(&mut chunk)?;
        let size = u64::from(u32::from_le_bytes(chunk[4..].try_into()?));
        let payload = position + 8;
        ensure!(size <= length - payload, "Truncated export WAV chunk");
        if &chunk[..4] == b"fmt " {
            ensure!(size >= 40, "Export WAV has no extensible speaker layout");
            let mut format = [0_u8; 40];
            file.read_exact(&mut format)?;
            // WAVEFORMATEX (18 bytes), valid bits (2), mask (4), subtype GUID (16).
            const MASK_OFFSET: u64 = 20;
            const IEEE_FLOAT_GUID: [u8; 16] =
                [3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113];
            ensure!(
                u16::from_le_bytes(format[..2].try_into()?) == 0xfffe
                    && u16::from_le_bytes(format[2..4].try_into()?) == spec.channels
                    && u32::from_le_bytes(format[4..8].try_into()?) == spec.sample_rate
                    && u16::from_le_bytes(format[14..16].try_into()?) == 32
                    && u16::from_le_bytes(format[16..18].try_into()?) >= 22
                    && u16::from_le_bytes(format[18..20].try_into()?) == 32
                    && format[24..] == IEEE_FLOAT_GUID,
                "Export WAV has an unexpected extensible float32 format"
            );
            file.seek(SeekFrom::Start(payload + MASK_OFFSET))?;
            file.write_all(&mask.to_le_bytes())?;
            return Ok(());
        }
        position = payload + size + (size & 1);
    }
    bail!("Export WAV has no fmt chunk")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn streaming_blocks_reorder_speakers_without_changing_frames() {
        use audio::Channel::{FrontLeft, FrontRight, TopRearRight};

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("stream.wav");
        let cancel = AtomicBool::new(false);
        let (temporary, canonical) = prepare_streaming_export(
            48_000,
            &[TopRearRight, FrontRight, FrontLeft],
            &destination,
            &cancel,
            |writer| {
                writer.write_frames(&[])?;
                writer.write_frames(&[3.0, 2.0, 1.0])?;
                writer.write_frames(&[6.0, 5.0, 4.0, 9.0, 8.0, 7.0])
            },
        )
        .unwrap();
        assert_eq!(canonical, destination);
        assert!(!destination.exists());
        let mut reader = hound::WavReader::open(temporary.path()).unwrap();
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.spec().channels, 3);
        assert_eq!(reader.spec().bits_per_sample, 32);
        assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
        assert_eq!(reader.duration(), 3);
        assert_eq!(
            reader
                .samples::<f32>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]
        );
        let bytes = fs::read(temporary.path()).unwrap();
        assert_eq!(&bytes[40..44], &((1_u32 << 17) | 3).to_le_bytes());
    }

    #[test]
    fn streaming_failures_remove_unpublished_files() {
        use audio::Channel::{FrontLeft, FrontRight};

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("failed.wav");
        let cancel = AtomicBool::new(false);
        let layout = [FrontLeft, FrontRight];
        for samples in [&[0.0][..], &[0.0, f32::NAN], &[f32::INFINITY, 0.0]] {
            assert!(
                prepare_streaming_export(48_000, &layout, &destination, &cancel, |writer| {
                    writer.write_frames(samples)
                })
                .is_err()
            );
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
        // A producer cannot accidentally hide a failed write and publish its prefix.
        assert!(
            prepare_streaming_export(48_000, &layout, &destination, &cancel, |writer| {
                writer.write_frames(&[0.0, 0.0])?;
                assert!(writer.write_frames(&[f32::NAN, 0.0]).is_err());
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(
            prepare_streaming_export(48_000, &layout, &destination, &cancel, |writer| {
                writer.write_frames(&[0.0, 0.0])?;
                bail!("deterministic producer failure")
            })
            .is_err()
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(
            prepare_streaming_export(48_000, &layout, &destination, &cancel, |writer| {
                writer.write_frames(&[0.0, 0.0])?;
                cancel.store(true, Ordering::Release);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn invalid_stream_formats_never_invoke_the_producer() {
        use audio::Channel::{FrontLeft, FrontRight};

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("invalid.wav");
        let cancel = AtomicBool::new(false);
        for (rate, layout) in [
            (0, &[FrontLeft][..]),
            (48_000, &[]),
            (48_000, &[FrontLeft, FrontLeft]),
            (u32::MAX, &[FrontLeft, FrontRight]),
        ] {
            assert!(
                prepare_streaming_export(rate, layout, &destination, &cancel, |_| {
                    panic!("invalid stream reached producer")
                })
                .is_err()
            );
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }
}
