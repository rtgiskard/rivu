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
    let canonical = export_destination(destination)?;
    ensure!(
        canonical == destination,
        "Export destination parent changed after admission"
    );
    check_cancel(cancel)?;
    let mut temporary = tempfile::NamedTempFile::new_in(
        canonical
            .parent()
            .context("Export destination has no parent")?,
    )?;
    let spec = hound::WavSpec {
        channels: source.channels(),
        sample_rate: source.sample_rate(),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    // WAVE masks describe samples in ascending speaker-bit order, unlike an
    // in-memory custom layout, which may retain any decoder-provided ordering.
    let mut order: Vec<_> = (0..source.layout().len()).collect();
    order.sort_unstable_by_key(|&index| source.layout()[index].standard_index());
    let mask = source.layout().iter().fold(0_u32, |mask, speaker| {
        mask | (1 << speaker.standard_index())
    });
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(temporary.as_file_mut()), spec)?;
    let mut block = vec![0.0; 8192 * usize::from(source.channels())];
    let mut position = 0;
    progress(0, source.frames());
    while position < source.frames() {
        check_cancel(cancel)?;
        let frames = source.read_frames(position, &mut block);
        debug_assert!(frames > 0);
        for frame in block[..frames * order.len()].chunks_exact(order.len()) {
            for &index in &order {
                writer.write_sample(frame[index])?;
            }
        }
        position += frames as u64;
        progress(position, source.frames());
    }
    writer.finalize()?;
    set_wav_channel_mask(temporary.as_file_mut(), spec, mask)?;
    temporary.as_file().sync_all()?;
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
