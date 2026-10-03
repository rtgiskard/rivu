use crate::audio::probe;
use crate::model::{Playlist, Track};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
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
}

#[derive(Clone, Debug)]
pub struct ScanRecord {
    pub path: PathBuf,
    pub size: u64,
    pub modified_ns: i64,
    pub fingerprint: Option<String>,
    pub media: MediaInfo,
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    pub roots: Vec<PathBuf>,
    pub records: Vec<ScanRecord>,
    pub errors: Vec<String>,
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

/// Scan directories and files without touching the database or audio output.
/// Callers should run this on a worker. A file whose size and mtime are unchanged
/// reuses its cached fingerprint and probe result; all other files are hashed and
/// probed, with individual failures retained in `errors`.
pub fn scan_paths(paths: &[PathBuf], known: &[KnownFile]) -> Result<ScanResult> {
    let mut known_by_path = HashMap::with_capacity(known.len());
    for item in known {
        known_by_path.insert(item.path.as_path(), item);
    }
    let mut files = Vec::new();
    let mut seen_files = std::collections::HashSet::new();
    let mut roots = Vec::with_capacity(paths.len());
    for requested in paths {
        let root = match fs::canonicalize(requested) {
            Ok(path) => path,
            Err(error) => {
                roots.push(absolute_hint(requested));
                files.push(Err(anyhow!("{}: {error}", requested.display())));
                continue;
            }
        };
        roots.push(root.clone());
        let metadata = match fs::metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) => {
                files.push(Err(anyhow!("{}: {error}", requested.display())));
                continue;
            }
        };
        if metadata.is_file() {
            if is_audio_path(&root) {
                if seen_files.insert(root.clone()) {
                    files.push(Ok(root));
                }
            } else {
                files.push(Err(anyhow!(
                    "{} is not a supported audio file or contains no audio",
                    requested.display()
                )));
            }
            continue;
        }
        if metadata.is_dir() {
            for entry in WalkDir::new(&root).follow_links(false).into_iter() {
                match entry {
                    Ok(entry) if entry.file_type().is_file() && is_audio_path(entry.path()) => {
                        let path = entry.into_path();
                        if seen_files.insert(path.clone()) {
                            files.push(Ok(path));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => files.push(Err(anyhow!("{}: {error}", root.display()))),
                }
            }
        }
    }
    let mut result = ScanResult {
        roots,
        records: Vec::with_capacity(files.len()),
        errors: Vec::new(),
    };
    for file in files {
        let path = match file {
            Ok(path) => path,
            Err(error) => {
                result.errors.push(format!("{error:#}"));
                continue;
            }
        };
        match scan_file(&path, known_by_path.get(path.as_path()).copied()) {
            Ok(record) => result.records.push(record),
            Err(error) => result.errors.push(format!("{}: {error:#}", path.display())),
        }
    }
    result.records.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn scan_file(path: &Path, known: Option<&KnownFile>) -> Result<ScanRecord> {
    let metadata =
        fs::metadata(path).with_context(|| format!("read metadata for {}", path.display()))?;
    let size = metadata.len();
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    if let Some(known) = known
        && known.size == size
        && known.modified_ns == modified_ns
        && let (Some(fingerprint), Some(media)) = (&known.fingerprint, &known.media)
    {
        return Ok(ScanRecord {
            path: path.to_path_buf(),
            size,
            modified_ns,
            fingerprint: Some(fingerprint.clone()),
            media: media.clone(),
        });
    }
    let media = probe(path).with_context(|| "probe audio")?;
    let fingerprint = hash_file(path)?;
    Ok(ScanRecord {
        path: path.to_path_buf(),
        size,
        modified_ns,
        fingerprint: Some(fingerprint),
        media,
    })
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
            | "aiff"
            | "aif"
            | "alac"
            | "ape"
            | "flac"
            | "m4a"
            | "mka"
            | "mkv"
            | "mp3"
            | "mp4"
            | "oga"
            | "ogg"
            | "opus"
            | "wav"
            | "wave"
            | "webm"
    )
}

#[derive(Clone, Debug)]
pub struct M3uItem {
    pub path: PathBuf,
    pub name: Option<String>,
}

/// Read a UTF-8 M3U/M3U8 file. Relative paths are resolved against its parent.
pub fn import_m3u(path: &Path) -> Result<Vec<M3uItem>> {
    let bytes = fs::read(path).with_context(|| format!("read playlist {}", path.display()))?;
    let text = std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
        .with_context(|| format!("playlist {} is not UTF-8", path.display()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut pending_name = None;
    let mut entries = Vec::new();
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
        entries.push(M3uItem {
            path: item_path,
            name: pending_name.take(),
        });
    }
    Ok(entries)
}

/// Write an UTF-8 M3U8, retaining every playlist entry including duplicates.
pub fn export_m3u(path: &Path, playlist: &Playlist, tracks: &[Track]) -> Result<()> {
    let by_id: HashMap<i64, &Track> = tracks.iter().map(|track| (track.id, track)).collect();
    let file = File::create(path).with_context(|| format!("create playlist {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(b"#EXTM3U\n")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for entry in &playlist.entries {
        let Some(track) = by_id.get(&entry.track_id) else {
            continue;
        };
        let duration = track
            .duration
            .map(|seconds| seconds.max(0.0).round() as i64)
            .unwrap_or(-1);
        let display_name = if track.artist.is_empty() {
            track.title.as_str()
        } else if track.title.is_empty() {
            track.artist.as_str()
        } else {
            // Keep the conventional artist - title label without changing stored metadata.
            // This is deliberately borrowed, avoiding an allocation for the common case below.
            writer.write_all(
                format!("#EXTINF:{duration},{} - {}\n", track.artist, track.title).as_bytes(),
            )?;
            write_relative_path(&mut writer, parent, &track.path)?;
            continue;
        };
        writer.write_all(format!("#EXTINF:{duration},{display_name}\n").as_bytes())?;
        write_relative_path(&mut writer, parent, &track.path)?;
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
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn path_relative_to(parent: &Path, target: &Path) -> PathBuf {
    if !target.is_absolute() {
        return target.to_path_buf();
    }
    let parent = absolute_hint(parent);
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
