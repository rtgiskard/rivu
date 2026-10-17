use super::ScanCancellation;
use super::input::{MAX_CUE_BYTES, read_input};
use super::paths::{is_cue_path, logical_path};
use crate::model::Track;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct M3uItem {
    pub path: PathBuf,
    pub name: Option<String>,
    pub cue_track: Option<u32>,
}

// Large enough for substantial music collections, while bounding hostile inputs.
pub(crate) const MAX_IMPORT_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_IMPORT_ENTRIES: usize = 100_000;
const MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct PlaylistImport {
    pub items: Vec<M3uItem>,
    pub sheets: HashMap<PathBuf, crate::cue::CueSheet>,
}

/// Import a bounded CUE sheet in sheet order, or a UTF-8 M3U/M3U8 playlist.
pub fn import_playlist(path: &Path) -> Result<Vec<M3uItem>> {
    Ok(prepare_playlist_import(path, &ScanCancellation::default(), None)?.items)
}

/// Read a bounded UTF-8 M3U/M3U8, expanding complete CUE sheets in order.
pub fn import_m3u(path: &Path) -> Result<Vec<M3uItem>> {
    Ok(prepare_import(path, false, &ScanCancellation::default(), None)?.items)
}

pub(crate) fn prepare_playlist_import(
    path: &Path,
    cancellation: &ScanCancellation,
    roots: Option<&[PathBuf]>,
) -> Result<PlaylistImport> {
    prepare_import(path, is_cue_path(path), cancellation, roots)
}

fn prepare_import(
    path: &Path,
    cue: bool,
    cancellation: &ScanCancellation,
    roots: Option<&[PathBuf]>,
) -> Result<PlaylistImport> {
    let path = logical_path(path)?;
    let mut result = PlaylistImport {
        items: Vec::new(),
        sheets: HashMap::new(),
    };
    let playlist_bytes = if cue {
        None
    } else {
        Some(
            read_input(&path, MAX_IMPORT_FILE_BYTES, cancellation)
                .with_context(|| format!("Reading import playlist {}", path.display()))?,
        )
    };
    let mut bytes_read = playlist_bytes.as_ref().map_or(0, Vec::len);
    let mut retained = 0;
    let mut expanded_bytes = 0;
    let mut add_cue =
        |path: &Path, result: &mut PlaylistImport, expanded_bytes: &mut usize| -> Result<()> {
            cancellation.check()?;
            if let Some(roots) = roots {
                ensure!(
                    roots.iter().any(|root| path.starts_with(root)),
                    "CUE sheet is outside configured library roots: {}",
                    path.display()
                );
            }
            if !result.sheets.contains_key(path) {
                let bytes = read_input(
                    path,
                    MAX_CUE_BYTES.min(MAX_IMPORT_BYTES - bytes_read),
                    cancellation,
                )
                .with_context(|| format!("Reading import CUE {}", path.display()))?;
                bytes_read += bytes.len();
                let sheet = crate::cue::parse_input(&bytes, path, cancellation)?;
                // Account for retained parsed paths/metadata, not merely source bytes.
                retained += path.as_os_str().len() + std::mem::size_of::<crate::cue::CueSheet>();
                retained += sheet.title.as_ref().map_or(0, String::len)
                    + sheet.performer.as_ref().map_or(0, String::len);
                for track in &sheet.tracks {
                    cancellation.check()?;
                    retained += std::mem::size_of::<crate::cue::CueTrack>()
                        + track.file.as_os_str().len()
                        + track.title.as_ref().map_or(0, String::len)
                        + track.performer.as_ref().map_or(0, String::len);
                }
                ensure!(
                    retained <= MAX_RETAINED_BYTES,
                    "import CUE metadata exceeds retained data limit"
                );
                result.sheets.insert(path.to_path_buf(), sheet);
            }
            let sheet = &result.sheets[path];
            ensure!(
                result.items.len() + sheet.tracks.len() <= MAX_IMPORT_ENTRIES,
                "import expansion exceeds {MAX_IMPORT_ENTRIES} entries"
            );
            for track in &sheet.tracks {
                cancellation.check()?;
                *expanded_bytes += std::mem::size_of::<M3uItem>()
                    + path.as_os_str().len()
                    + track.title.as_ref().map_or(0, String::len);
                ensure!(
                    *expanded_bytes <= MAX_RETAINED_BYTES,
                    "import expansion exceeds retained data limit"
                );
                result.items.push(M3uItem {
                    path: path.to_path_buf(),
                    name: track.title.clone(),
                    cue_track: Some(track.number),
                });
            }
            Ok(())
        };
    if cue {
        add_cue(&path, &mut result, &mut expanded_bytes)?;
    } else {
        let bytes = playlist_bytes.as_ref().unwrap();
        let text = std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .with_context(|| format!("Decoding import playlist {} as UTF-8", path.display()))?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut pending_name = None;
        for raw_line in text.lines() {
            cancellation.check()?;
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
            let item_path = logical_path(&if item_path.is_absolute() {
                item_path
            } else {
                parent.join(item_path)
            })?;
            if let Some(roots) = roots {
                ensure!(
                    roots.iter().any(|root| item_path.starts_with(root)),
                    "Import source is outside configured library roots: {}",
                    item_path.display()
                );
            }
            let name = pending_name.take();
            if is_cue_path(&item_path) {
                add_cue(&item_path, &mut result, &mut expanded_bytes)?;
            } else {
                ensure!(
                    result.items.len() < MAX_IMPORT_ENTRIES,
                    "import expansion exceeds {MAX_IMPORT_ENTRIES} entries"
                );
                expanded_bytes += std::mem::size_of::<M3uItem>()
                    + item_path.as_os_str().len()
                    + name.as_ref().map_or(0, String::len);
                ensure!(
                    expanded_bytes <= MAX_RETAINED_BYTES,
                    "import expansion exceeds retained data limit"
                );
                result.items.push(M3uItem {
                    path: item_path,
                    name,
                    cue_track: None,
                });
            }
        }
    }
    cancellation.check()?;
    ensure!(
        !result.items.is_empty(),
        "Import playlist contains no entries"
    );
    Ok(result)
}

/// Export a database-ordered stream of unique playlist tracks atomically.
/// SQLite enforces uniqueness; retain only the current CUE validation run.
pub fn export_m3u<I>(path: &Path, tracks: I) -> Result<()>
where
    I: IntoIterator<Item = Result<Track>>,
{
    let path = logical_path(path)?;
    let parent = path.parent().context("playlist has no parent directory")?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("create temporary playlist")?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        writer.write_all(b"#EXTM3U\n")?;
        let mut cue_run: Option<(PathBuf, crate::cue::CueSheet, usize)> = None;
        for item in tracks {
            let mut track = item?;
            track.path = logical_path(&track.path)?;
            if let Some(cue) = &mut track.cue {
                cue.sheet = logical_path(&cue.sheet)?;
            }
            if cue_run.is_some() {
                let mut finished = false;
                if let Some((sheet_path, sheet, index)) = cue_run.as_mut() {
                    let expected = sheet
                        .tracks
                        .get(*index)
                        .context("CUE sheet track sequence ended unexpectedly")?;
                    let matches = track.cue.as_ref().is_some_and(|segment| {
                        segment.sheet == *sheet_path
                            && segment.number == expected.number
                            && segment.start_frame == expected.start_frame
                            && segment.end_frame == expected.end_frame
                    });
                    if !matches {
                        bail!(
                            "Standard M3U cannot represent partial or reordered CUE tracks; include every track from {} in sheet order",
                            sheet_path.display()
                        );
                    }
                    if logical_path(&expected.file)? != track.path {
                        bail!(
                            "CUE source changed; rescan {} before exporting",
                            sheet_path.display()
                        );
                    }
                    *index += 1;
                    finished = *index == sheet.tracks.len();
                }
                if finished {
                    cue_run = None;
                }
                continue;
            }
            if let Some(cue) = &track.cue {
                let cancellation = ScanCancellation::default();
                let bytes = read_input(&cue.sheet, MAX_CUE_BYTES, &cancellation)
                    .with_context(|| format!("Reading export CUE {}", cue.sheet.display()))?;
                let sheet = crate::cue::parse_input(&bytes, &cue.sheet, &cancellation)?;
                let expected = sheet
                    .tracks
                    .first()
                    .context("CUE sheet contains no tracks")?;
                if cue.number != expected.number
                    || cue.start_frame != expected.start_frame
                    || cue.end_frame != expected.end_frame
                    || logical_path(&expected.file)? != track.path
                {
                    bail!(
                        "Standard M3U cannot represent partial or reordered CUE tracks; include every track from {} in sheet order",
                        cue.sheet.display()
                    );
                }
                write_relative_path(&mut writer, parent, &cue.sheet)?;
                if sheet.tracks.len() > 1 {
                    cue_run = Some((cue.sheet.clone(), sheet, 1));
                }
                continue;
            }
            let duration = track
                .duration
                .map(|seconds| seconds.max(0.0).round() as i64)
                .unwrap_or(-1);
            write!(writer, "#EXTINF:{duration},")?;
            if !track.artist.is_empty() {
                write_single_line(&mut writer, &track.artist)?;
                if !track.title.is_empty() {
                    writer.write_all(b" - ")?;
                }
            }
            write_single_line(&mut writer, &track.title)?;
            writer.write_all(b"\n")?;
            write_relative_path(&mut writer, parent, &track.path)?;
        }
        if let Some((sheet, _, _)) = cue_run {
            bail!(
                "Standard M3U cannot represent partial or reordered CUE tracks; include every track from {} in sheet order",
                sheet.display()
            );
        }
        writer.flush().context("flush playlist")?;
    }
    temporary
        .persist(&path)
        .with_context(|| format!("replace playlist {}", path.display()))?;
    Ok(())
}

fn write_single_line(writer: &mut impl Write, text: &str) -> io::Result<()> {
    for (index, part) in text.split(['\r', '\n']).enumerate() {
        if index != 0 {
            writer.write_all(b" ")?;
        }
        writer.write_all(part.as_bytes())?;
    }
    Ok(())
}

fn write_relative_path(writer: &mut impl Write, parent: &Path, track: &Path) -> Result<()> {
    let display_path = path_relative_to(parent, track);
    let text = display_path.to_str().ok_or_else(|| {
        anyhow!(
            "playlist path is not valid UTF-8: {}",
            display_path.display()
        )
    })?;
    if text.contains(['\r', '\n']) {
        bail!(
            "Standard M3U cannot represent a path containing CR or LF: {}",
            display_path.display()
        );
    }
    // A leading hash is an M3U directive, not a filename, unless prefixed.
    if text.starts_with('#') {
        writer.write_all(b"./")?;
    }
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn path_relative_to(parent: &Path, target: &Path) -> PathBuf {
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
