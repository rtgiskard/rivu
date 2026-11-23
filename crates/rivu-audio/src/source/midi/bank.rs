//! One immutable process-wide SoundFont result, including permanent failures.

use anyhow::{Context, Result, ensure};
use crossbeam_channel::{Receiver, TryRecvError};
use rustysynth::SoundFont;
use std::{
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, OnceLock},
};

type SoundFontResult = OnceLock<Result<Arc<SoundFont>, String>>;

pub(super) struct SoundFontLoad {
    result: Arc<SoundFontResult>,
    pub(super) ready: Receiver<()>,
}

impl SoundFontLoad {
    pub(super) fn start(load: impl FnOnce() -> Result<Arc<SoundFont>> + Send + 'static) -> Self {
        let result = Arc::new(SoundFontResult::new());
        let published = Arc::clone(&result);
        let (done, ready) = crossbeam_channel::bounded(0);
        let started = std::thread::Builder::new()
            .name("rivu-midi-bank".into())
            .spawn(move || {
                let _ = published.set(load().map_err(|error| format!("{error:#}")));
                // Disconnection is a one-shot broadcast, not a consumable wake.
                drop(done);
            });
        if let Err(error) = started {
            let _ = result.set(Err(format!("Starting MIDI SoundFont loader: {error}")));
        }
        Self { result, ready }
    }

    #[cfg(test)]
    pub(super) fn completed(bank: Arc<SoundFont>) -> Self {
        let (_, ready) = crossbeam_channel::bounded(0);
        Self {
            result: Arc::new(OnceLock::from(Ok(bank))),
            ready,
        }
    }

    pub(super) fn poll(&self) -> Result<Option<&Arc<SoundFont>>> {
        if self.ready.try_recv() == Err(TryRecvError::Empty) {
            return Ok(None);
        }
        self.result
            .get()
            .context("MIDI SoundFont loader stopped without a result")?
            .as_ref()
            .map(Some)
            .map_err(|error| anyhow::anyhow!("{error}"))
    }
}

// One load and one cached result per process, including failures. Dropping a
// source abandons its wait, not the process-wide bank needed by other sources.
pub(super) static BANK: LazyLock<Arc<SoundFontLoad>> =
    LazyLock::new(|| Arc::new(SoundFontLoad::start(load_soundfont)));

fn load_soundfont() -> Result<Arc<SoundFont>> {
    let path = std::env::var_os("RIVU_MIDI_SOUNDFONT").map_or_else(
        || {
            [
                "/usr/share/soundfonts/FluidR3_GM.sf2",
                "/usr/share/sounds/sf2/FluidR3_GM.sf2",
                "/usr/share/sounds/sf2/default-GM.sf2",
            ]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .map(PathBuf::from)
        },
        |path| Some(PathBuf::from(path)),
    ).context("No General MIDI SoundFont found; install a GM .sf2 or set RIVU_MIDI_SOUNDFONT and restart Rivu")?;
    let (file, metadata) = rivu_utils::regular_file::RegularFile::open_with_metadata(&path)
        .with_context(|| format!("Opening MIDI SoundFont {}", path.display()))?;
    let mut reader = BufReader::new(file.into_file());
    validate_bank_chunks(&mut reader, metadata.len())
        .with_context(|| format!("Validating MIDI SoundFont {}", path.display()))?;
    reader.rewind()?;
    let bank = SoundFont::new(&mut reader)
        .with_context(|| format!("Loading MIDI SoundFont {}", path.display()))?;
    Ok(Arc::new(bank))
}

pub(super) fn validate_bank_chunks(reader: &mut (impl Read + Seek), len: u64) -> Result<()> {
    ensure!(len <= 512 * 1024 * 1024, "SoundFont exceeds 512 MiB");
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;
    let end = 8 + u64::from(u32::from_le_bytes(header[4..8].try_into()?));
    ensure!(
        &header[..4] == b"RIFF" && &header[8..] == b"sfbk" && end <= len,
        "Invalid SoundFont RIFF header"
    );
    let mut cursor = 12;
    let mut tables = [(0, 0); 6];
    // Mirror the parser's list boundaries without reading or allocating samples.
    for kind in [b"INFO", b"sdta", b"pdta"] {
        ensure!(cursor + 12 <= end, "Truncated SoundFont list header");
        reader.read_exact(&mut header)?;
        let list_end = cursor + 8 + u64::from(u32::from_le_bytes(header[4..8].try_into()?));
        cursor += 12;
        ensure!(
            &header[..4] == b"LIST" && &header[8..] == kind && (cursor..=end).contains(&list_end),
            "Invalid SoundFont list"
        );
        if kind != b"sdta" {
            ensure!(
                list_end - cursor <= 4 * 1024 * 1024,
                "SoundFont metadata list exceeds 4 MiB"
            );
        }
        while cursor < list_end {
            ensure!(cursor + 8 <= list_end, "Truncated SoundFont chunk header");
            reader.read_exact(&mut header[..8])?;
            let size = u64::from(u32::from_le_bytes(header[4..8].try_into()?));
            let id = &header[..4];
            if kind == b"INFO" && matches!(id, b"ifil" | b"iver") {
                ensure!(size == 4, "Invalid SoundFont version chunk size");
            }
            if let Some(index) = [b"phdr", b"pbag", b"pgen", b"inst", b"ibag", b"igen"]
                .iter()
                .position(|tag| *tag == id)
            {
                ensure!(tables[index] == (0, 0), "Duplicate SoundFont table");
                tables[index] = (cursor + 8, size);
            }
            cursor += 8 + size;
            ensure!(cursor <= list_end, "SoundFont chunk extends past its list");
            reader.seek(SeekFrom::Start(cursor))?;
        }
    }
    for (set, record, zone_offset) in [(0, 38, 24), (3, 22, 20)] {
        let [headers, bags, (_, generator_bytes)] = tables[set..set + 3].try_into()?;
        validate_table_indices(reader, headers, record, zone_offset, bags.1 / 4)?;
        ensure!(
            generator_bytes != 0 && generator_bytes.is_multiple_of(4),
            "Invalid SoundFont generator table size"
        );
        // The last generator is a terminator, so the largest legal exclusive
        // boundary is the number of records minus one.
        validate_table_indices(reader, bags, 4, 0, generator_bytes / 4)?;
    }
    Ok(())
}

// Header records point into bags; bags point into the generator table. The
// dependency builds ranges from adjacent records, with no bounds checks.
fn validate_table_indices(
    reader: &mut (impl Read + Seek),
    (offset, size): (u64, u64),
    record: usize,
    index_offset: usize,
    limit: u64,
) -> Result<()> {
    ensure!(
        size.is_multiple_of(record as u64) && size / record as u64 <= 65_536,
        "Invalid SoundFont table size"
    );
    reader.seek(SeekFrom::Start(offset))?;
    let mut row = [0; 38];
    let mut previous = 0;
    for _ in 0..size / record as u64 {
        reader.read_exact(&mut row[..record])?;
        let index = u64::from(u16::from_le_bytes(
            row[index_offset..index_offset + 2].try_into()?,
        ));
        ensure!(
            (previous..limit).contains(&index),
            "Invalid SoundFont table index"
        );
        previous = index;
    }
    Ok(())
}
