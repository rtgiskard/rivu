//! Test-only PCM files written through one buffered RIFF writer.

use std::path::Path;

pub fn write_i16_wav(
    path: &Path,
    channels: u16,
    sample_rate: u32,
    samples: impl IntoIterator<Item = i16>,
) -> hound::Result<()> {
    write_wav(
        path,
        channels,
        sample_rate,
        16,
        hound::SampleFormat::Int,
        samples,
    )
}

pub fn write_float_wav(
    path: &Path,
    channels: u16,
    sample_rate: u32,
    samples: impl IntoIterator<Item = f32>,
) -> hound::Result<()> {
    write_wav(
        path,
        channels,
        sample_rate,
        32,
        hound::SampleFormat::Float,
        samples,
    )
}

fn write_wav<S: hound::Sample>(
    path: &Path,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    sample_format: hound::SampleFormat,
    samples: impl IntoIterator<Item = S>,
) -> hound::Result<()> {
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample,
            sample_format,
        },
    )?;
    for sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()
}

/// A temporary WAV file with the given suffix, e.g. `""` for an extensionless probe.
#[cfg(test)]
pub(crate) fn i16_wav_file(
    suffix: &str,
    channels: u16,
    sample_rate: u32,
    samples: impl IntoIterator<Item = i16>,
) -> tempfile::NamedTempFile {
    let file = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
    write_i16_wav(file.path(), channels, sample_rate, samples).unwrap();
    file
}

#[cfg(test)]
pub(crate) fn float_wav_file(
    suffix: &str,
    channels: u16,
    sample_rate: u32,
    samples: impl IntoIterator<Item = f32>,
) -> tempfile::NamedTempFile {
    let file = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
    write_float_wav(file.path(), channels, sample_rate, samples).unwrap();
    file
}
