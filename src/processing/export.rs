use super::render::check_cancel;
use anyhow::{Context, Result, bail, ensure};
use rivu_audio as audio;
use rivu_dsp::equalizer::iir::Equalizer;
use rivu_protocol::model::{AudioExportEncoding, AudioExportFormat, AudioExportRequest};
use std::os::fd::AsRawFd;
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

#[cfg(test)]
pub(super) fn prepare_export(
    source: &audio::PcmAudio,
    destination: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    prepare_encoded_export(source, destination, None, cancel, progress)
}

pub(super) fn prepare_encoded_export(
    source: &audio::PcmAudio,
    destination: &Path,
    request: Option<&AudioExportRequest>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    prepare_streaming_encoded_export(
        source.sample_rate(),
        source.layout(),
        destination,
        request,
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

enum Sink<'a> {
    Wav {
        writer: hound::WavWriter<std::io::BufWriter<&'a mut fs::File>>,
        bits: u16,
        noise: Dither,
    },
    Encoded(audio::AudioEncoder),
}
impl Sink<'_> {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        match self {
            Self::Wav {
                writer,
                bits,
                noise,
            } => {
                let bytes = u64::from(*bits / 8);
                let remaining = (u64::from(u32::MAX) - 128) / bytes - u64::from(writer.len());
                ensure!(
                    samples.len() as u64 <= remaining,
                    "Export exceeds the RIFF/WAVE size limit"
                );
                if *bits == 32 {
                    ensure!(
                        samples.iter().all(|sample| sample.is_finite()),
                        "Export contains a non-finite sample"
                    );
                    for &sample in samples {
                        writer.write_sample(sample)?;
                    }
                } else {
                    audio::validate_integer_samples(samples, *bits)?;
                    for &sample in samples {
                        writer.write_sample(noise.quantize(sample, *bits))?;
                    }
                }
                Ok(())
            }
            Self::Encoded(encoder) => encoder.write(samples),
        }
    }
    fn finish(self) -> Result<()> {
        match self {
            Self::Wav { writer, .. } => {
                writer.finalize()?;
                Ok(())
            }
            Self::Encoded(mut encoder) => encoder.finish(),
        }
    }
}

/// Deterministic TPDF dither: two independent uniform draws per output sample.
/// Only dither excursions at representable endpoints saturate; signal overload
/// is rejected before quantization. State is continuous across PCM blocks.
struct Dither(u64);
impl Dither {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let value = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (value >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
    }
    fn quantize(&mut self, sample: f32, bits: u16) -> i32 {
        let scale = (1_u32 << (bits - 1)) as f64;
        let noise = self.uniform() - self.uniform();
        (f64::from(sample) * scale + noise)
            .round()
            .clamp(-scale, scale - 1.0) as i32
    }
}

pub(super) struct ExportWriter<'a> {
    sink: Sink<'a>,
    cancel: &'a AtomicBool,
    order: [usize; 32],
    channels: usize,
    reorder: bool,
    eq: Option<Equalizer>,
    converter: Option<audio::ExportConverter>,
    block: Vec<f32>,
    failed: bool,
}
impl ExportWriter<'_> {
    pub(super) fn write_frames(&mut self, samples: &[f32]) -> Result<()> {
        ensure!(!self.failed, "Export writer failed a previous block");
        self.failed = true;
        ensure!(
            samples.len().is_multiple_of(self.channels),
            "Export contains an incomplete interleaved frame"
        );
        for input in samples.chunks(8192 * self.channels) {
            check_cancel(self.cancel)?;
            let samples = if self.eq.is_none() && !self.reorder {
                input
            } else {
                self.block.clear();
                self.block.extend_from_slice(input);
                if let Some(eq) = &mut self.eq {
                    eq.process(&mut self.block)?;
                }
                // EQ channel targets refer to source order; reorder only afterwards.
                if self.reorder {
                    for frame in self.block.chunks_exact_mut(self.channels) {
                        let mut sorted = [0.0_f32; 32];
                        for (out, &index) in sorted.iter_mut().zip(&self.order[..self.channels]) {
                            *out = frame[index];
                        }
                        frame.copy_from_slice(&sorted[..self.channels]);
                    }
                }
                &self.block
            };
            if let Some(converter) = &mut self.converter {
                self.sink.write(converter.convert(samples)?)?;
            } else {
                self.sink.write(samples)?;
            }
        }
        check_cancel(self.cancel)?;
        self.failed = false;
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        ensure!(!self.failed, "Cannot publish audio after a failed block");
        if let Some(converter) = &mut self.converter {
            check_cancel(self.cancel)?;
            self.sink.write(converter.finish()?)?;
        }
        check_cancel(self.cancel)?;
        self.sink.finish()
    }
}

#[cfg(test)]
pub(super) fn prepare_streaming_export(
    sample_rate: u32,
    layout: &[audio::Channel],
    destination: &Path,
    cancel: &AtomicBool,
    produce: impl FnOnce(&mut ExportWriter<'_>) -> Result<()>,
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    prepare_streaming_encoded_export(sample_rate, layout, destination, None, cancel, produce)
}

pub(super) fn prepare_streaming_encoded_export(
    sample_rate: u32,
    layout: &[audio::Channel],
    destination: &Path,
    request: Option<&AudioExportRequest>,
    cancel: &AtomicBool,
    produce: impl FnOnce(&mut ExportWriter<'_>) -> Result<()>,
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
    let defaults = AudioExportEncoding::default();
    let options = request.map_or(&defaults, |request| &request.encoding);
    options.validate()?;
    let mut order = [0; 32];
    let mut input_mask = 0_u32;
    for (index, speaker) in layout.iter().enumerate() {
        let bit = 1_u32 << speaker.standard_index();
        ensure!(
            input_mask & bit == 0,
            "Export has a duplicate speaker: {speaker:?}"
        );
        input_mask |= bit;
        order[index] = index;
    }
    order[..layout.len()].sort_unstable_by_key(|&index| layout[index].standard_index());
    let rate = options.sample_rate.unwrap_or(sample_rate);
    let mask = match options.channels {
        Some(1) => 4,
        Some(2) => 3,
        _ => input_mask,
    };
    let channels = mask.count_ones() as u16;
    options.validate_stream(rate, channels)?;
    ensure!(
        rate.checked_mul(u32::from(channels) * 4).is_some(),
        "Export sample rate exceeds the WAVE byte-rate limit"
    );
    let converter = if sample_rate != rate || input_mask != mask {
        Some(audio::ExportConverter::new(
            sample_rate,
            u64::from(input_mask),
            rate,
            u64::from(mask),
        )?)
    } else {
        None
    };
    let eq = request
        .and_then(|request| request.monitor_eq.as_ref())
        .map(|eq| Equalizer::new(eq, sample_rate, layout.len()))
        .transpose()?;
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: options.pcm_bits,
        sample_format: if options.pcm_bits == 32 {
            hound::SampleFormat::Float
        } else {
            hound::SampleFormat::Int
        },
    };
    let mut temporary = tempfile::NamedTempFile::new_in(
        canonical
            .parent()
            .context("Export destination has no parent")?,
    )?;
    // Reopen the already-owned inode, not its directory entry. No encoder may
    // replace or follow a raced scratch path supplied by somebody else.
    let descriptor = PathBuf::from(format!("/proc/self/fd/{}", temporary.as_file().as_raw_fd()));
    let sink = if options.format == AudioExportFormat::Wav {
        Sink::Wav {
            writer: hound::WavWriter::new(std::io::BufWriter::new(temporary.as_file_mut()), spec)?,
            bits: options.pcm_bits,
            noise: Dither(0x7269_7675_6469_7468),
        }
    } else {
        Sink::Encoded(audio::AudioEncoder::new(
            &descriptor,
            rate,
            u64::from(mask),
            options,
        )?)
    };
    let reorder = order[..layout.len()]
        .iter()
        .enumerate()
        .any(|(i, &index)| i != index);
    let capacity = if reorder || eq.is_some() {
        8192 * layout.len()
    } else {
        0
    };
    let mut writer = ExportWriter {
        sink,
        cancel,
        order,
        channels: layout.len(),
        reorder,
        eq,
        converter,
        block: Vec::with_capacity(capacity),
        failed: false,
    };
    produce(&mut writer)?;
    writer.finish()?;
    if options.format == AudioExportFormat::Wav {
        set_wav_channel_mask(temporary.as_file_mut(), spec, mask, cancel)?;
    }
    check_cancel(cancel)?;
    Ok((temporary, canonical))
}

/// Hound guesses extensible masks; ordinary PCM mono/stereo has implicit masks.
/// Locate and validate the actual RIFF fmt chunk before replacing dwChannelMask
/// on the same unpublished file; hound still owns all encoding and finalization.
fn set_wav_channel_mask(
    file: &mut fs::File,
    spec: hound::WavSpec,
    mask: u32,
    cancel: &AtomicBool,
) -> Result<()> {
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
            if size == 16 && spec.sample_format == hound::SampleFormat::Int {
                if (spec.channels == 1 && mask == 4) || (spec.channels == 2 && mask == 3) {
                    return Ok(());
                }
                // An unusual mono/stereo speaker assignment needs extensible
                // PCM. Grow only this header, moving data backwards in bounded blocks.
                let mut format = [0_u8; 40];
                file.read_exact(&mut format[..16])?;
                let mut block = [0_u8; 65536];
                let insertion = payload + 16;
                let mut end = length;
                ensure!(
                    length + 24 - 8 <= u64::from(u32::MAX),
                    "Export exceeds RIFF size limit"
                );
                file.set_len(length + 24)?;
                while end > insertion {
                    check_cancel(cancel)?;
                    let count = (end - insertion).min(block.len() as u64) as usize;
                    let start = end - count as u64;
                    file.seek(SeekFrom::Start(start))?;
                    file.read_exact(&mut block[..count])?;
                    file.seek(SeekFrom::Start(start + 24))?;
                    file.write_all(&block[..count])?;
                    end = start;
                }
                format[..2].copy_from_slice(&0xfffe_u16.to_le_bytes());
                format[16..18].copy_from_slice(&22_u16.to_le_bytes());
                format[18..20].copy_from_slice(&spec.bits_per_sample.to_le_bytes());
                format[20..24].copy_from_slice(&mask.to_le_bytes());
                format[24..]
                    .copy_from_slice(&[1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]);
                file.seek(SeekFrom::Start(position + 4))?;
                file.write_all(&40_u32.to_le_bytes())?;
                file.write_all(&format)?;
                file.seek(SeekFrom::Start(4))?;
                file.write_all(&((length + 24 - 8) as u32).to_le_bytes())?;
                return Ok(());
            }
            ensure!(size >= 40, "Export WAV has no extensible speaker layout");
            let mut format = [0_u8; 40];
            file.read_exact(&mut format)?;
            // WAVEFORMATEX (18 bytes), valid bits (2), mask (4), subtype GUID (16).
            const MASK_OFFSET: u64 = 20;
            let subtype = if spec.sample_format == hound::SampleFormat::Float {
                3
            } else {
                1
            };
            let guid = [
                subtype, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113,
            ];
            ensure!(
                u16::from_le_bytes(format[..2].try_into()?) == 0xfffe
                    && u16::from_le_bytes(format[2..4].try_into()?) == spec.channels
                    && u32::from_le_bytes(format[4..8].try_into()?) == spec.sample_rate
                    && u16::from_le_bytes(format[14..16].try_into()?) == spec.bits_per_sample
                    && u16::from_le_bytes(format[16..18].try_into()?) >= 22
                    && u16::from_le_bytes(format[18..20].try_into()?) == spec.bits_per_sample
                    && format[24..] == guid,
                "Export WAV has an unexpected extensible format"
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

    fn request(path: &Path, bits: u16) -> AudioExportRequest {
        AudioExportRequest {
            source: rivu_protocol::model::AudioExportSource::Original { track_id: 1 },
            path: path.to_owned(),
            monitor_eq: None,
            encoding: AudioExportEncoding {
                pcm_bits: bits,
                ..Default::default()
            },
        }
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn resampled_wav_finalization_preserves_empty_and_short_sources() {
        if !audio::export_encoder_available(AudioExportFormat::Flac) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("resampled.wav");
        let cancel = AtomicBool::new(false);
        for (input_rate, output_rate) in [(44_100, 48_000), (48_000, 44_100)] {
            let mut request = request(&destination, 32);
            request.encoding.sample_rate = Some(output_rate);
            for frames in [0, 1, 8193] {
                let source = vec![0.125; frames];
                let (temporary, _) = prepare_streaming_encoded_export(
                    input_rate,
                    &[audio::Channel::FrontCenter],
                    &destination,
                    Some(&request),
                    &cancel,
                    |writer| writer.write_frames(&source),
                )
                .unwrap();
                let mut reader = hound::WavReader::open(temporary.path()).unwrap();
                let target =
                    (frames as u64 * u64::from(output_rate)).div_ceil(u64::from(input_rate));
                assert_eq!(u64::from(reader.duration()), target);
                assert_eq!(reader.spec().sample_rate, output_rate);
                let samples = reader
                    .samples::<f32>()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert_eq!(samples.is_empty(), frames == 0);
                assert!(samples.iter().all(|sample| (sample - 0.125).abs() < 1e-5));
                assert!(!destination.exists());
            }
        }
    }

    #[test]
    fn integer_wav_dither_is_block_invariant_and_rejects_overload() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("integer.wav");
        let cancel = AtomicBool::new(false);
        let source = vec![0.00001_f32; 32771];
        for bits in [16, 24] {
            let request = request(&destination, bits);
            let render = |chunks: usize| {
                let (temporary, _) = prepare_streaming_encoded_export(
                    48_000,
                    &[audio::Channel::FrontCenter],
                    &destination,
                    Some(&request),
                    &cancel,
                    |writer| {
                        for chunk in source.chunks(chunks) {
                            writer.write_frames(chunk)?;
                        }
                        Ok(())
                    },
                )
                .unwrap();
                let mut reader = hound::WavReader::open(temporary.path()).unwrap();
                assert_eq!(reader.spec().bits_per_sample, bits);
                assert_eq!(reader.duration(), source.len() as u32);
                reader
                    .samples::<i32>()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            };
            let one = render(source.len());
            assert_eq!(one, render(37));
            let scale = (1_u32 << (bits - 1)) as f64;
            assert!(
                one.iter()
                    .zip(&source)
                    .all(|(&q, &s)| (f64::from(q) / scale - f64::from(s)).abs() <= 1.5 / scale)
            );
            let mean = one
                .iter()
                .map(|&q| f64::from(q) / scale - f64::from(source[0]))
                .sum::<f64>()
                / one.len() as f64;
            assert!(mean.abs() < 0.02 / scale);
            assert!(
                prepare_streaming_encoded_export(
                    48_000,
                    &[audio::Channel::FrontCenter],
                    &destination,
                    Some(&request),
                    &cancel,
                    |writer| writer.write_frames(&[1.25])
                )
                .is_err()
            );
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn integer_mono_nonstandard_speaker_is_not_relabelled() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("lfe.wav");
        let request = request(&destination, 16);
        let (temporary, _) = prepare_streaming_encoded_export(
            48_000,
            &[audio::Channel::Lfe],
            &destination,
            Some(&request),
            &AtomicBool::new(false),
            |writer| writer.write_frames(&[0.125; 4097]),
        )
        .unwrap();
        let bytes = fs::read(temporary.path()).unwrap();
        assert_eq!(&bytes[40..44], &8_u32.to_le_bytes());
        let mut reader = hound::WavReader::open(temporary.path()).unwrap();
        assert_eq!(reader.duration(), 4097);
        assert!(
            reader
                .samples::<i16>()
                .all(|s| (s.unwrap() - 4096).abs() <= 1)
        );
    }

    #[test]
    fn monitor_snapshot_is_applied_exactly_once_and_float_peaks_survive() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("eq.wav");
        let mut request = request(&destination, 32);
        let eq = rivu_dsp::equalizer::config::EqualizerConfig {
            enabled: true,
            output_gain_db: 6.0,
            ..Default::default()
        };
        request.monitor_eq = Some(eq.clone());
        let input = [0.75; 400];
        let mut expected = input;
        Equalizer::new(&eq, 48_000, 1)
            .unwrap()
            .process(&mut expected)
            .unwrap();
        let (temporary, _) = prepare_streaming_encoded_export(
            48_000,
            &[audio::Channel::FrontCenter],
            &destination,
            Some(&request),
            &AtomicBool::new(false),
            |writer| {
                for chunk in input.chunks(31) {
                    writer.write_frames(chunk)?;
                }
                Ok(())
            },
        )
        .unwrap();
        let mut reader = hound::WavReader::open(temporary.path()).unwrap();
        let actual = reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(actual, expected);
        assert!(actual.iter().any(|s| *s > 1.0));
    }

    #[test]
    fn codec_constraints_require_explicit_conversion_before_production() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("encoded.opus");
        let mut request = request(&destination, 32);
        request.encoding.format = AudioExportFormat::Opus;
        let error = prepare_streaming_encoded_export(
            44_100,
            &[audio::Channel::FrontCenter],
            &destination,
            Some(&request),
            &AtomicBool::new(false),
            |_| panic!("invalid conversion reached producer"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("explicitly select 48000"));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[cfg(all(test, feature = "ffmpeg"))]
#[test]
fn encoded_export_cancellation_discards_every_unpublished_file() {
    use std::sync::atomic::Ordering;
    for format in [
        AudioExportFormat::Flac,
        AudioExportFormat::Mp3,
        AudioExportFormat::Opus,
    ] {
        if !audio::export_encoder_available(format) {
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("cancelled.audio");
        let request = AudioExportRequest {
            source: rivu_protocol::model::AudioExportSource::Original { track_id: 1 },
            path: destination.clone(),
            monitor_eq: None,
            encoding: AudioExportEncoding {
                format,
                pcm_bits: 24,
                ..Default::default()
            },
        };
        let cancel = AtomicBool::new(false);
        let outcome = prepare_streaming_encoded_export(
            48_000,
            &[audio::Channel::FrontCenter],
            &destination,
            Some(&request),
            &cancel,
            |writer| {
                writer.write_frames(&[0.125; 8192])?;
                cancel.store(true, Ordering::Release);
                Ok(())
            },
        );
        assert!(outcome.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
