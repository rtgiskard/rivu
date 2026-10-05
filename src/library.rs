use crate::audio::probe;
use crate::model::{CueSegment, Playlist, Track};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

/// Decoded file metadata shared by scanning, persistence, and playback.
///
/// This value contains no decoder or audio-output state; probing supplies it and
/// the library caches it independently of the playback engine.
#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    /// Encoded audio bitrate in bits/s, when stream metadata supplies it.
    pub bitrate_bps: Option<u64>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    /// Source PCM/lossless precision, never the decoder's output sample width.
    pub bits_per_sample: Option<u32>,
    /// Original date precision is retained (e.g. year, year-month, or full date).
    pub release_date: Option<String>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
}

/// The filesystem information and cached probe result known to the database.
#[derive(Clone, Debug)]
pub struct KnownFile {
    pub track_id: i64,
    pub path: PathBuf,
    pub size: u64,
    pub modified_ns: i64,
    pub fingerprint: Option<String>,
    pub media: Option<MediaInfo>,
    pub cue: Option<CueSegment>,
}

#[derive(Clone, Debug)]
pub struct ScanRecord {
    pub path: PathBuf,
    pub size: u64,
    pub modified_ns: i64,
    pub fingerprint: Option<String>,
    pub media: MediaInfo,
    pub cue: Option<CueSegment>,
}
/// Core-owned mutable library catalog. The published `AppState` keeps immutable
/// snapshots; small playback-stat changes never mutate an `Arc` shared with a
/// frontend and therefore never trigger implicit copy-on-write.
pub struct LibraryState {
    tracks: Vec<Track>,
    dirty: bool,
}

impl LibraryState {
    pub fn new(tracks: Vec<Track>) -> Self {
        Self {
            tracks,
            dirty: false,
        }
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn replace(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        self.dirty = true;
    }

    pub fn update(&mut self, track_id: i64, update: impl FnOnce(&mut Track)) -> Result<()> {
        let index = self
            .tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .map_err(|_| anyhow!("Track not found: {track_id}"))?;
        update(&mut self.tracks[index]);
        self.dirty = true;
        Ok(())
    }

    pub fn snapshot(&mut self) -> Option<Arc<Vec<Track>>> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(Arc::new(self.tracks.clone()))
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    pub roots: Vec<PathBuf>,
    pub records: Vec<ScanRecord>,
    pub errors: Vec<String>,
    /// Whole sources represented by successfully scanned CUE sheets.
    pub suppressed_sources: Vec<PathBuf>,
}

impl ScanResult {
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn summary(&self) -> String {
        match (self.records.len(), self.errors.len()) {
            (records, 0) => format!("Found {records} audio files"),
            (records, errors) => format!("Found {records} audio files ({errors} errors)"),
        }
    }
}

/// Scan directories, audio files and CUE sheets without touching audio output.
/// CUE metadata is always reread; only unchanged full-source probe results may
/// be reused. Failed sheets contribute no partial tracks or source suppression.
pub fn scan_paths(
    paths: &[PathBuf],
    known: &[KnownFile],
    ffmpeg_enabled: bool,
) -> Result<ScanResult> {
    let mut known_by_path: HashMap<&Path, &KnownFile> = HashMap::with_capacity(known.len());
    for item in known {
        let cached = known_by_path.entry(item.path.as_path()).or_insert(item);
        if cached.cue.is_some() && item.cue.is_none() {
            *cached = item;
        }
    }
    let mut files = HashSet::new();
    let mut explicit_audio = HashSet::new();
    let mut result = ScanResult::default();
    for requested in paths {
        let root = match fs::canonicalize(requested) {
            Ok(path) => path,
            Err(error) => {
                result.roots.push(absolute_hint(requested));
                result
                    .errors
                    .push(format!("{}: {error}", requested.display()));
                continue;
            }
        };
        result.roots.push(root.clone());
        if root.is_file() {
            if is_audio_path(&root) {
                explicit_audio.insert(root.clone());
                files.insert(root);
            } else if is_cue_path(&root) {
                files.insert(root);
            } else {
                result.errors.push(format!(
                    "{} is not a supported audio file or CUE sheet",
                    requested.display()
                ));
            }
        } else if root.is_dir() {
            for entry in WalkDir::new(&root).follow_links(false) {
                match entry {
                    Ok(entry)
                        if entry.file_type().is_file()
                            && (is_audio_path(entry.path()) || is_cue_path(entry.path())) =>
                    {
                        match entry.path().canonicalize() {
                            Ok(path) => {
                                files.insert(path);
                            }
                            Err(error) => result
                                .errors
                                .push(format!("{}: {error}", entry.path().display())),
                        }
                    }
                    Ok(_) => {}
                    Err(error) => result.errors.push(format!("{}: {error}", root.display())),
                }
            }
        }
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort();
    let mut sources = HashMap::new();
    let mut suppressed = HashSet::new();
    for sheet in files.iter().filter(|path| is_cue_path(path)) {
        match scan_cue(sheet, &known_by_path, &mut sources, ffmpeg_enabled) {
            Ok(records) => {
                suppressed.extend(records.iter().map(|record| record.path.clone()));
                result.records.extend(records);
            }
            Err(error) => result
                .errors
                .push(format!("{}: {error:#}", sheet.display())),
        }
    }
    for path in files.iter().filter(|path| is_audio_path(path)) {
        if suppressed.contains(path) && !explicit_audio.contains(path) {
            continue;
        }
        match source_record(path, &known_by_path, &mut sources, ffmpeg_enabled) {
            Ok(record) => result.records.push(record.clone()),
            Err(error) => result.errors.push(format!("{}: {error:#}", path.display())),
        }
    }
    suppressed.retain(|path| !explicit_audio.contains(path));
    result.suppressed_sources = suppressed.into_iter().collect();
    result.suppressed_sources.sort();
    result.records.sort_by(|a, b| {
        let identity = |record: &ScanRecord| record.cue.as_ref().map(|cue| cue.number).unwrap_or(0);
        let a_path = a.cue.as_ref().map_or(&a.path, |cue| &cue.sheet);
        let b_path = b.cue.as_ref().map_or(&b.path, |cue| &cue.sheet);
        a_path
            .cmp(b_path)
            .then_with(|| identity(a).cmp(&identity(b)))
    });
    Ok(result)
}

fn source_record<'a>(
    path: &Path,
    known: &HashMap<&Path, &KnownFile>,
    sources: &'a mut HashMap<PathBuf, std::result::Result<ScanRecord, String>>,
    ffmpeg_enabled: bool,
) -> Result<&'a ScanRecord> {
    sources
        .entry(path.to_path_buf())
        .or_insert_with(|| {
            scan_file(path, known.get(path).copied(), ffmpeg_enabled)
                .map_err(|error| format!("{error:#}"))
        })
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn scan_cue(
    path: &Path,
    known: &HashMap<&Path, &KnownFile>,
    sources: &mut HashMap<PathBuf, std::result::Result<ScanRecord, String>>,
    ffmpeg_enabled: bool,
) -> Result<Vec<ScanRecord>> {
    let sheet = crate::cue::read(path)?;
    let mut records = Vec::with_capacity(sheet.tracks.len());
    for track in sheet.tracks {
        let source = track
            .file
            .canonicalize()
            .with_context(|| format!("resolve CUE source {}", track.file.display()))?;
        let mut record = source_record(&source, known, sources, ffmpeg_enabled)?.clone();
        let duration = record
            .media
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .with_context(|| format!("CUE source {} has no finite duration", source.display()))?;
        let start = track.start_frame as f64 / 75.0;
        let end = track
            .end_frame
            .map_or(duration, |frame| frame as f64 / 75.0);
        if start >= duration || end <= start || end > duration + 1.0 / 75.0 {
            bail!(
                "CUE track {} has invalid boundaries {start}..{end} for source duration {duration}",
                track.number
            );
        }
        record.media.duration = Some(end.min(duration) - start);
        record.media.track_number = Some(track.number);
        record.media.title = track
            .title
            .unwrap_or_else(|| format!("Track {:02}", track.number));
        if let Some(artist) = track.performer.as_ref().or(sheet.performer.as_ref()) {
            record.media.artist.clone_from(artist);
        }
        if let Some(album) = &sheet.title {
            record.media.album.clone_from(album);
        }
        record.cue = Some(CueSegment {
            sheet: path.to_path_buf(),
            number: track.number,
            start_frame: track.start_frame,
            end_frame: track.end_frame,
        });
        records.push(record);
    }
    Ok(records)
}

fn scan_file(path: &Path, known: Option<&KnownFile>, ffmpeg_enabled: bool) -> Result<ScanRecord> {
    let metadata =
        fs::metadata(path).with_context(|| format!("read metadata for {}", path.display()))?;
    let size = metadata.len();
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let unchanged = known.filter(|known| known.size == size && known.modified_ns == modified_ns);
    let media = match unchanged
        .filter(|known| known.cue.is_none())
        .and_then(|known| known.media.as_ref())
        .filter(|media| !ffmpeg_enabled || (media.channels != 0 && media.sample_rate != 0))
    {
        Some(media) => media.clone(),
        None => probe(path, ffmpeg_enabled).with_context(|| "probe audio")?,
    };
    let fingerprint = match unchanged.and_then(|known| known.fingerprint.as_ref()) {
        Some(fingerprint) => fingerprint.clone(),
        None => hash_file(path)?,
    };
    Ok(ScanRecord {
        path: path.to_path_buf(),
        size,
        modified_ns,
        fingerprint: Some(fingerprint),
        media,
        cue: None,
    })
}

fn is_cue_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cue"))
}

fn hash_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher).with_context(|| format!("hash {}", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn absolute_hint(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn is_audio_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "aac"
            | "ac3"
            | "aiff"
            | "aif"
            | "alac"
            | "ape"
            | "caf"
            | "dff"
            | "dsf"
            | "dts"
            | "dtshd"
            | "eac3"
            | "flac"
            | "m4a"
            | "mka"
            | "mkv"
            | "mlp"
            | "mp3"
            | "mp4"
            | "oga"
            | "ogg"
            | "opus"
            | "shn"
            | "tak"
            | "thd"
            | "truehd"
            | "tta"
            | "wav"
            | "wave"
            | "webm"
            | "wma"
    )
}

#[derive(Clone, Debug)]
pub struct M3uItem {
    pub path: PathBuf,
    pub name: Option<String>,
    pub cue_track: Option<u32>,
}

/// Import a CUE sheet in sheet order, or a UTF-8 M3U/M3U8 playlist.
pub fn import_playlist(path: &Path) -> Result<Vec<M3uItem>> {
    if is_cue_path(path) {
        cue_items(path)
    } else {
        import_m3u(path)
    }
}

fn cue_items(path: &Path) -> Result<Vec<M3uItem>> {
    Ok(crate::cue::read(path)?
        .tracks
        .into_iter()
        .map(|track| M3uItem {
            path: path.to_path_buf(),
            name: track.title,
            cue_track: Some(track.number),
        })
        .collect())
}

/// Read a UTF-8 M3U/M3U8 file, expanding bare CUE paths. Relative paths are
/// resolved against its parent. CUE paths always refer to the complete sheet.
pub fn import_m3u(path: &Path) -> Result<Vec<M3uItem>> {
    let bytes = fs::read(path).with_context(|| format!("read playlist {}", path.display()))?;
    let text = std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
        .with_context(|| format!("playlist {} is not UTF-8", path.display()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut pending_name = None;
    let mut entries = Vec::new();
    let mut sheets = HashMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            pending_name = info.split_once(',').map(|(_, name)| name.trim().to_owned());
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let item_path = PathBuf::from(line);
        let item_path = if item_path.is_absolute() {
            item_path
        } else {
            parent.join(item_path)
        };
        let name = pending_name.take();
        if is_cue_path(&item_path) {
            let items = match sheets.entry(item_path.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(cue_items(&item_path)?)
                }
            };
            entries.extend(items.iter().cloned());
        } else {
            entries.push(M3uItem {
                path: item_path,
                name,
                cue_track: None,
            });
        }
    }
    Ok(entries)
}

/// Write an UTF-8 M3U8, retaining only the first occurrence of each track ID.
/// Resolve the existing output directory physically before making paths relative,
/// so `..` and directory symlinks retain their filesystem meaning.
pub fn export_m3u(path: &Path, playlist: &Playlist, tracks: &[Track]) -> Result<()> {
    let by_id: HashMap<i64, &Track> = tracks.iter().map(|track| (track.id, track)).collect();
    let mut seen = HashSet::new();
    let entries: Vec<&Track> = playlist
        .entries
        .iter()
        .filter(|entry| seen.insert(entry.track_id))
        .map(|entry| {
            by_id
                .get(&entry.track_id)
                .copied()
                .context("Playlist contains a missing library track")
        })
        .collect::<Result<_>>()?;
    // Resolve every unique track before opening the destination. Standard M3U has
    // no syntax for selecting a CUE subtrack, so only complete sheet runs can
    // be represented without changing what will play on reimport.
    let mut sheets = HashMap::new();
    let mut output = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let track = entries[index];
        if let Some(cue) = &track.cue {
            let sheet = match sheets.entry(cue.sheet.as_path()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(crate::cue::read(&cue.sheet)?)
                }
            };
            for (offset, expected) in sheet.tracks.iter().enumerate() {
                let selected = entries.get(index + offset).copied();
                let matches = selected.is_some_and(|selected| {
                    selected.cue.as_ref().is_some_and(|segment| {
                        segment.sheet == cue.sheet
                            && segment.number == expected.number
                            && segment.start_frame == expected.start_frame
                            && segment.end_frame == expected.end_frame
                    })
                });
                if !matches {
                    bail!(
                        "Standard M3U cannot represent partial or reordered CUE tracks; include every track from {} in sheet order",
                        cue.sheet.display()
                    );
                }
                if expected.file.canonicalize()? != selected.unwrap().path {
                    bail!(
                        "CUE source changed; rescan {} before exporting",
                        cue.sheet.display()
                    );
                }
            }
            index += sheet.tracks.len();
        } else {
            index += 1;
        }
        output.push(track);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = parent
        .canonicalize()
        .with_context(|| format!("resolve playlist directory {}", parent.display()))?;
    let file = File::create(path).with_context(|| format!("create playlist {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(b"#EXTM3U\n")?;
    for track in output {
        if let Some(cue) = &track.cue {
            write_relative_path(&mut writer, &parent, &cue.sheet)?;
            continue;
        }
        let duration = track
            .duration
            .map(|seconds| seconds.max(0.0).round() as i64)
            .unwrap_or(-1);
        if !track.artist.is_empty() && !track.title.is_empty() {
            writeln!(
                writer,
                "#EXTINF:{duration},{} - {}",
                track.artist, track.title
            )?;
        } else {
            let name = if track.artist.is_empty() {
                &track.title
            } else {
                &track.artist
            };
            writeln!(writer, "#EXTINF:{duration},{name}")?;
        }
        write_relative_path(&mut writer, &parent, &track.path)?;
    }
    writer.flush().context("flush playlist")?;
    Ok(())
}

fn write_relative_path(writer: &mut BufWriter<File>, parent: &Path, track: &Path) -> Result<()> {
    let display_path = path_relative_to(parent, track);
    let text = display_path.to_str().ok_or_else(|| {
        anyhow!(
            "playlist path is not valid UTF-8: {}",
            display_path.display()
        )
    })?;
    // A leading hash is an M3U directive, not a filename, unless prefixed.
    if text.starts_with('#') {
        writer.write_all(b"./")?;
    }
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn path_relative_to(parent: &Path, target: &Path) -> PathBuf {
    if !target.is_absolute() {
        return target.to_path_buf();
    }
    let parent_components: Vec<_> = parent.components().collect();
    let target_components: Vec<_> = target.components().collect();
    let mut common = 0;
    while common < parent_components.len()
        && common < target_components.len()
        && parent_components[common] == target_components[common]
    {
        common += 1;
    }
    let mut relative = PathBuf::new();
    for component in &parent_components[common..] {
        if !matches!(component, std::path::Component::Prefix(_)) {
            relative.push("..");
        }
    }
    for component in &target_components[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PlaylistEntry;

    // Mono PCM WAVs with exactly 100 samples per CD frame, no audio device.
    fn wav(path: &Path, frames: u32) -> Result<()> {
        let data_len = frames * 100 * 2;
        let mut file = File::create(path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&(36 + data_len).to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16_u32.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&7500_u32.to_le_bytes())?;
        file.write_all(&15000_u32.to_le_bytes())?;
        file.write_all(&2_u16.to_le_bytes())?;
        file.write_all(&16_u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&data_len.to_le_bytes())?;
        file.write_all(&vec![0; data_len as usize])?;
        Ok(())
    }

    fn two_track_sheet(path: &Path) -> Result<()> {
        fs::write(
            path,
            "TITLE \"Album\"\nPERFORMER \"Sheet artist\"\nFILE \"audio.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"First\"\n    PERFORMER \"Track artist\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 00 00:00:60\n    INDEX 01 00:01:00\n",
        )?;
        Ok(())
    }

    fn known_records(records: &[ScanRecord]) -> Vec<KnownFile> {
        records
            .iter()
            .enumerate()
            .map(|(index, record)| KnownFile {
                track_id: index as i64 + 1,
                path: record.path.clone(),
                size: record.size,
                modified_ns: record.modified_ns,
                fingerprint: record.fingerprint.clone(),
                media: Some(record.media.clone()),
                cue: record.cue.clone(),
            })
            .collect()
    }

    fn track_from_record(id: i64, record: &ScanRecord) -> Track {
        Track {
            id,
            path: record.path.clone(),
            fingerprint: record.fingerprint.clone(),
            title: record.media.title.clone(),
            artist: record.media.artist.clone(),
            album: record.media.album.clone(),
            duration: record.media.duration,
            bitrate_bps: record.media.bitrate_bps,
            track_number: record.media.track_number,
            disc_number: record.media.disc_number,
            bits_per_sample: record.media.bits_per_sample,
            release_date: record.media.release_date.clone(),
            favorite: false,
            codec: record.media.codec.clone(),
            channels: record.media.channels,
            sample_rate: record.media.sample_rate,
            cue: record.cue.clone(),
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    #[test]
    fn cue_single_source_tracks_have_relative_duration_and_metadata() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let audio = directory.path().join("audio.wav");
        let sheet = directory.path().join("album.CUE");
        wav(&audio, 225)?;
        two_track_sheet(&sheet)?;
        let result = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.records.len(), 2);
        let first = &result.records[0];
        let second = &result.records[1];
        assert_eq!(first.path, audio.canonicalize()?);
        assert_eq!(second.path, first.path);
        assert_eq!(first.media.title, "First");
        assert_eq!(first.media.artist, "Track artist");
        assert_eq!(second.media.title, "Track 02");
        assert_eq!(second.media.artist, "Sheet artist");
        assert_eq!(first.media.album, "Album");
        assert_eq!(first.media.duration, Some(1.0));
        assert_eq!(second.media.duration, Some(2.0));
        assert_eq!(first.media.bitrate_bps, Some(120_000));
        assert_eq!(second.media.bitrate_bps, first.media.bitrate_bps);
        assert_eq!(first.media.bits_per_sample, Some(16));
        assert_eq!(first.media.track_number, Some(1));
        assert_eq!(second.media.track_number, Some(2));
        assert_eq!(
            first.cue.as_ref().unwrap(),
            &CueSegment {
                sheet: sheet.canonicalize()?,
                number: 1,
                start_frame: 0,
                end_frame: Some(75),
            }
        );
        assert_eq!(second.cue.as_ref().unwrap().start_frame, 75);
        assert_eq!(second.cue.as_ref().unwrap().end_frame, None);
        Ok(())
    }

    #[test]
    fn cue_multiple_files_reset_indexes_and_keep_sheet_order() -> Result<()> {
        let directory = tempfile::tempdir()?;
        wav(&directory.path().join("z.wav"), 150)?;
        wav(&directory.path().join("a.wav"), 225)?;
        let sheet = directory.path().join("multi.cue");
        fs::write(
            &sheet,
            "FILE \"z.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00\nFILE \"a.wav\" WAVE\nTRACK 03 AUDIO\nINDEX 01 00:00:00\nTRACK 04 AUDIO\nINDEX 01 00:02:00\n",
        )?;
        let result = scan_paths(&[sheet], &[], false)?;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.records.len(), 4);
        assert_eq!(
            result
                .records
                .iter()
                .map(|record| record.cue.as_ref().unwrap().number)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            result
                .records
                .iter()
                .map(|record| record.media.duration)
                .collect::<Vec<_>>(),
            [Some(1.0), Some(1.0), Some(2.0), Some(1.0)]
        );
        assert_eq!(result.records[1].cue.as_ref().unwrap().end_frame, None);
        assert_eq!(result.records[2].cue.as_ref().unwrap().start_frame, 0);
        assert_eq!(result.suppressed_sources.len(), 2);
        Ok(())
    }

    #[test]
    fn cue_rescan_rereads_sheet_and_never_reuses_segment_as_full_media() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let audio = directory.path().join("audio.wav");
        let sheet = directory.path().join("album.cue");
        wav(&audio, 225)?;
        two_track_sheet(&sheet)?;
        let original = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
        assert!(original.errors.is_empty(), "{:?}", original.errors);
        let known = known_records(&original.records);
        let full = scan_paths(std::slice::from_ref(&audio), &known, false)?;
        assert!(full.errors.is_empty(), "{:?}", full.errors);
        assert_eq!(full.records.len(), 1);
        assert_eq!(full.records[0].media.duration, Some(3.0));
        assert!(full.records[0].cue.is_none());
        fs::write(
            &sheet,
            "TITLE \"Edited album\"\nFILE \"audio.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"Renamed\"\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:02:00\n",
        )?;
        for cache in [&known, &known_records(&full.records)] {
            let rescanned = scan_paths(std::slice::from_ref(&sheet), cache, false)?;
            assert!(rescanned.errors.is_empty(), "{:?}", rescanned.errors);
            assert_eq!(rescanned.records.len(), 2);
            assert_eq!(rescanned.records[0].media.title, "Renamed");
            assert_eq!(rescanned.records[0].media.album, "Edited album");
            assert_eq!(
                rescanned.records[0].media.artist,
                full.records[0].media.artist
            );
            assert_eq!(rescanned.records[0].media.duration, Some(2.0));
            assert_eq!(rescanned.records[1].media.duration, Some(1.0));
            assert_eq!(
                rescanned.records[0].fingerprint,
                original.records[0].fingerprint
            );
        }
        Ok(())
    }

    #[test]
    fn cue_missing_source_and_out_of_bounds_fail_atomically() -> Result<()> {
        let directory = tempfile::tempdir()?;
        wav(&directory.path().join("audio.wav"), 150)?;
        let sheet = directory.path().join("broken.cue");
        for invalid in [
            "FILE \"missing.wav\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n",
            "TRACK 02 AUDIO\nINDEX 01 00:02:00\n",
            "TRACK 02 AUDIO\nINDEX 01 00:02:02\n",
            "TRACK 02 AUDIO\nINDEX 01 00:00:00\n",
        ] {
            fs::write(
                &sheet,
                format!("FILE \"audio.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n{invalid}"),
            )?;
            let result = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
            assert!(result.records.is_empty());
            assert_eq!(result.errors.len(), 1);
            assert!(result.suppressed_sources.is_empty());
            let directory_scan = scan_paths(&[directory.path().to_path_buf()], &[], false)?;
            assert_eq!(directory_scan.records.len(), 1);
            assert!(directory_scan.records[0].cue.is_none());
            assert!(directory_scan.suppressed_sources.is_empty());
        }
        Ok(())
    }

    #[test]
    fn cue_directory_suppresses_sources_but_explicit_audio_survives() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let audio = directory.path().join("audio.wav");
        let sheet = directory.path().join("album.cue");
        wav(&audio, 225)?;
        wav(&directory.path().join("unrelated.wav"), 75)?;
        two_track_sheet(&sheet)?;
        let result = scan_paths(&[directory.path().to_path_buf(), sheet.clone()], &[], false)?;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.records.len(), 3);
        assert_eq!(
            result
                .records
                .iter()
                .filter(|record| record.cue.is_none())
                .count(),
            1
        );
        assert_eq!(result.suppressed_sources, [audio.canonicalize()?]);
        let explicit = scan_paths(
            &[directory.path().to_path_buf(), audio.clone(), sheet, audio],
            &[],
            false,
        )?;
        assert!(explicit.errors.is_empty(), "{:?}", explicit.errors);
        assert_eq!(explicit.records.len(), 4);
        assert_eq!(
            explicit
                .records
                .iter()
                .filter(|record| record.cue.is_none())
                .count(),
            2
        );
        assert!(explicit.suppressed_sources.is_empty());
        Ok(())
    }

    #[test]
    fn standard_cue_paths_roundtrip_complete_sheets_with_track_deduplication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let audio = directory.path().join("audio.wav");
        let sheet = directory.path().join("#album.cue");
        wav(&audio, 225)?;
        two_track_sheet(&sheet)?;
        let scan = scan_paths(&[sheet.clone(), audio], &[], false)?;
        let tracks: Vec<_> = scan
            .records
            .iter()
            .enumerate()
            .map(|(index, record)| track_from_record(index as i64 + 1, record))
            .collect();
        let mut sequence: Vec<_> = tracks
            .iter()
            .filter(|track| track.cue.is_some())
            .map(|track| track.id)
            .collect();
        sequence.extend(sequence.clone());
        sequence.insert(1, sequence[0]);
        sequence.push(tracks.iter().find(|track| track.cue.is_none()).unwrap().id);
        let playlist = Playlist {
            id: 1,
            name: "Standard CUE references".into(),
            entries: sequence
                .iter()
                .enumerate()
                .map(|(index, id)| PlaylistEntry {
                    id: index as i64 + 1,
                    track_id: *id,
                })
                .collect(),
        };
        let path = directory.path().join("playlist.m3u8");
        export_m3u(&path, &playlist, &tracks)?;
        let imported = import_playlist(&path)?;
        let expected = [sequence[0], sequence[2], *sequence.last().unwrap()];
        assert_eq!(imported.len(), expected.len());
        for (item, id) in imported.iter().zip(expected) {
            let track = tracks.iter().find(|track| track.id == id).unwrap();
            assert_eq!(item.cue_track, track.cue.as_ref().map(|cue| cue.number));
            assert_eq!(
                item.path.canonicalize()?,
                *track.cue.as_ref().map_or(&track.path, |cue| &cue.sheet)
            );
        }
        // Ordinary path entries, usable without interpreting player-specific tags.
        let text = fs::read_to_string(&path)?;
        assert_eq!(
            text.lines().filter(|line| *line == "./#album.cue").count(),
            1
        );
        Ok(())
    }

    #[test]
    fn unrepresentable_cue_exports_preserve_existing_destination() -> Result<()> {
        let directory = tempfile::tempdir()?;
        wav(&directory.path().join("audio.wav"), 225)?;
        let sheet = directory.path().join("album.cue");
        two_track_sheet(&sheet)?;
        let scan = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
        let tracks: Vec<_> = scan
            .records
            .iter()
            .enumerate()
            .map(|(index, record)| track_from_record(index as i64 + 1, record))
            .collect();
        let path = directory.path().join("existing.m3u8");
        fs::write(&path, "original playlist\n")?;
        for sequence in [vec![1], vec![2], vec![2, 1]] {
            let playlist = Playlist {
                id: 1,
                name: "Partial CUE".into(),
                entries: sequence
                    .into_iter()
                    .enumerate()
                    .map(|(index, track_id)| PlaylistEntry {
                        id: index as i64 + 1,
                        track_id,
                    })
                    .collect(),
            };
            assert!(export_m3u(&path, &playlist, &tracks).is_err());
            assert_eq!(fs::read_to_string(&path)?, "original playlist\n");
        }
        // A sheet changed outside Rivu must not silently change exported ranges.
        let playlist = Playlist {
            id: 1,
            name: "Stale sheet".into(),
            entries: vec![
                PlaylistEntry { id: 1, track_id: 1 },
                PlaylistEntry { id: 2, track_id: 2 },
            ],
        };
        fs::write(
            &sheet,
            fs::read_to_string(&sheet)?.replace("00:01:00", "00:02:00"),
        )?;
        assert!(export_m3u(&path, &playlist, &tracks).is_err());
        assert_eq!(fs::read_to_string(&path)?, "original playlist\n");
        Ok(())
    }

    fn assert_playlist_roundtrip(path: &Path) -> Result<()> {
        let directory = path.parent().unwrap().canonicalize()?;
        let tracks: Vec<_> = ["#song.wav", "other song.wav"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| Track {
                id: index as i64 + 1,
                path: directory.join(name),
                fingerprint: None,
                title: name.into(),
                artist: if index == 0 {
                    "Artist".into()
                } else {
                    String::new()
                },
                album: String::new(),
                duration: Some(2.0),
                bitrate_bps: None,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
                favorite: false,
                codec: "wav".into(),
                channels: 2,
                sample_rate: 48_000,
                missing: false,
                play_count: 0,
                last_played: None,
                cue: None,
            })
            .collect();
        for track in &tracks {
            fs::write(&track.path, b"playlist path fixture")?;
        }
        let playlist = Playlist {
            id: 1,
            name: "Roundtrip".into(),
            entries: [2, 1, 2, 1]
                .into_iter()
                .enumerate()
                .map(|(index, track_id)| PlaylistEntry {
                    id: index as i64 + 1,
                    track_id,
                })
                .collect(),
        };
        export_m3u(path, &playlist, &tracks)?;
        let text = fs::read_to_string(path)?;
        assert_eq!(
            text.lines().filter(|line| *line == "./#song.wav").count(),
            1
        );
        let imported = import_m3u(path)?;
        assert_eq!(imported.len(), 2);
        for (item, index) in imported.iter().zip([1, 0]) {
            assert_eq!(item.path.canonicalize()?, tracks[index].path);
            assert_eq!(item.cue_track, None);
        }
        assert_eq!(imported[0].name.as_deref(), Some("other song.wav"));
        assert_eq!(imported[1].name.as_deref(), Some("Artist - #song.wav"));
        Ok(())
    }

    #[test]
    fn m3u_roundtrip_preserves_hash_paths_and_first_track_order() -> Result<()> {
        let directory = tempfile::tempdir()?;
        assert_playlist_roundtrip(&directory.path().join("playlist.m3u8"))
    }

    #[test]
    fn m3u_roundtrip_resolves_parent_components() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("sub"))?;
        assert_playlist_roundtrip(&directory.path().join("sub/../playlist.m3u8"))
    }

    #[cfg(unix)]
    #[test]
    fn m3u_roundtrip_resolves_symlink_before_parent_component() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let nested = directory.path().join("physical/nested");
        fs::create_dir_all(&nested)?;
        std::os::unix::fs::symlink(nested, directory.path().join("link"))?;
        assert_playlist_roundtrip(&directory.path().join("link/../playlist.m3u8"))
    }
}
