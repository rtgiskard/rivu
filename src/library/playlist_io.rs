use super::paths::{is_cue_path, logical_path};
use crate::model::Track;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

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
    let path = logical_path(path)?;
    Ok(crate::cue::read(&path)?
        .tracks
        .into_iter()
        .map(|track| M3uItem {
            path: path.clone(),
            name: track.title,
            cue_track: Some(track.number),
        })
        .collect())
}

/// Read a UTF-8 M3U/M3U8 file, expanding bare CUE paths. Relative paths are
/// resolved against its parent. CUE paths always refer to the complete sheet.
pub fn import_m3u(path: &Path) -> Result<Vec<M3uItem>> {
    let path = logical_path(path)?;
    let bytes = fs::read(&path).with_context(|| format!("read playlist {}", path.display()))?;
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
        let item_path = logical_path(&item_path)?;
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
                let sheet = crate::cue::read(&cue.sheet)?;
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

fn write_relative_path(writer: &mut impl Write, parent: &Path, track: &Path) -> Result<()> {
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
