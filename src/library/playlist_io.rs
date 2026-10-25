use super::ScanCancellation;
use super::input::{MAX_CUE_BYTES, read_input_async};
use super::paths::{is_cue_path, logical_path};
use crate::core::tasks::BlockingPool;
use anyhow::{Context, Result, anyhow, bail, ensure};
use futures::{FutureExt, Stream, StreamExt};
use rivu_protocol::model::PlaylistExportTrack;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct M3uItem {
    pub path: PathBuf,
    pub cue_track: Option<u32>,
}

// Large enough for substantial music collections, while bounding hostile inputs.
pub(crate) const MAX_IMPORT_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_IMPORT_ENTRIES: usize = 100_000;
const MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;

const EXPORT_BUFFER_BYTES: usize = 256 * 1024;
const COOPERATIVE_ROWS: usize = 256;

/// Keep formatting on the async task, but submit only bounded file writes.
/// The buffer and descriptor return after every write, including I/O errors.
struct ExportWriter<'a> {
    workers: &'a BlockingPool,
    file: Option<std::fs::File>,
    bytes: Vec<u8>,
    cancellation: ScanCancellation,
}

impl ExportWriter<'_> {
    async fn write_all(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let count = bytes.len().min(EXPORT_BUFFER_BYTES - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.bytes.len() == EXPORT_BUFFER_BYTES {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.cancellation.check()?;
        if self.bytes.is_empty() {
            return Ok(());
        }
        let mut file = self.file.take().context("playlist writer has no file")?;
        let mut bytes = std::mem::take(&mut self.bytes);
        let (file, bytes, result) = self
            .workers
            .execute(move || {
                let result = file.write_all(&bytes);
                bytes.clear();
                (file, bytes, result)
            })
            .await
            .context("Playlist file write worker panicked")?;
        self.file = Some(file);
        self.bytes = bytes;
        result.context("write playlist")?;
        self.cancellation.check()
    }
}

pub(crate) struct PlaylistImport {
    pub items: Vec<M3uItem>,
    pub sheets: HashMap<PathBuf, super::cue::CueSheet>,
}

/// Import a bounded CUE sheet in sheet order, or a UTF-8 M3U/M3U8 playlist.
#[cfg(test)]
pub fn import_playlist(path: &Path) -> Result<Vec<M3uItem>> {
    let workers = &BlockingPool::new(4);
    Ok(test_io(prepare_playlist_import(
        workers,
        path,
        &ScanCancellation::default(),
        None,
    ))?
    .items)
}

/// Read a bounded UTF-8 M3U/M3U8, expanding complete CUE sheets in order.
#[cfg(test)]
pub fn import_m3u(path: &Path) -> Result<Vec<M3uItem>> {
    let workers = &BlockingPool::new(4);
    Ok(test_io(prepare_import(
        workers,
        path,
        false,
        &ScanCancellation::default(),
        None,
    ))?
    .items)
}

#[cfg(test)]
fn test_io<T>(future: impl Future<Output = T>) -> T {
    futures::executor::block_on(future)
}

pub(crate) async fn prepare_playlist_import(
    workers: &BlockingPool,
    path: &Path,
    cancellation: &ScanCancellation,
    roots: Option<&[PathBuf]>,
) -> Result<PlaylistImport> {
    prepare_import(workers, path, is_cue_path(path), cancellation, roots).await
}

struct ImportBuilder {
    result: PlaylistImport,
    bytes_read: usize,
    retained: usize,
    expanded_bytes: usize,
}

impl ImportBuilder {
    fn add_sheet(
        &mut self,
        path: PathBuf,
        sheet: Option<super::cue::CueSheet>,
        cancellation: &ScanCancellation,
    ) -> Result<()> {
        if let Some(sheet) = sheet {
            self.retained += path.as_os_str().len()
                + std::mem::size_of::<super::cue::CueSheet>()
                + sheet.title.as_ref().map_or(0, String::len)
                + sheet.performer.as_ref().map_or(0, String::len);
            for track in &sheet.tracks {
                cancellation.check()?;
                self.retained += std::mem::size_of::<super::cue::CueTrack>()
                    + track.file.as_os_str().len()
                    + track.title.as_ref().map_or(0, String::len)
                    + track.performer.as_ref().map_or(0, String::len);
            }
            ensure!(
                self.retained <= MAX_RETAINED_BYTES,
                "import CUE metadata exceeds retained data limit"
            );
            self.result.sheets.insert(path.clone(), sheet);
        }
        let sheet = &self.result.sheets[&path];
        ensure!(
            self.result.items.len() + sheet.tracks.len() <= MAX_IMPORT_ENTRIES,
            "import expansion exceeds {MAX_IMPORT_ENTRIES} entries"
        );
        for track in &sheet.tracks {
            cancellation.check()?;
            self.expanded_bytes += std::mem::size_of::<M3uItem>() + path.as_os_str().len();
            ensure!(
                self.expanded_bytes <= MAX_RETAINED_BYTES,
                "import expansion exceeds retained data limit"
            );
            self.result.items.push(M3uItem {
                path: path.clone(),
                cue_track: Some(track.number),
            });
        }
        Ok(())
    }
}

async fn prepare_import(
    workers: &BlockingPool,
    path: &Path,
    cue: bool,
    cancellation: &ScanCancellation,
    roots: Option<&[PathBuf]>,
) -> Result<PlaylistImport> {
    cancellation.check()?;
    let path = logical_path(path)?;
    let mut builder = ImportBuilder {
        result: PlaylistImport {
            items: Vec::new(),
            sheets: HashMap::new(),
        },
        bytes_read: 0,
        retained: 0,
        expanded_bytes: 0,
    };
    let paths = if cue {
        vec![path]
    } else {
        let bytes = read_input_async(workers, &path, MAX_IMPORT_FILE_BYTES, cancellation)
            .await
            .with_context(|| format!("Reading import playlist {}", path.display()))?;
        builder.bytes_read = bytes.len();
        let cancellation = cancellation.clone();
        let roots = roots.map(<[PathBuf]>::to_vec);
        // UTF-8 validation, line parsing and logical path expansion are bounded
        // CPU work, not filesystem I/O on the async driver.
        workers
            .execute(move || -> Result<Vec<PathBuf>> {
                let text =
                    std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
                        .with_context(|| {
                            format!("Decoding import playlist {} as UTF-8", path.display())
                        })?;
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let mut paths = Vec::new();
                let mut retained = 0;
                for raw_line in text.lines() {
                    cancellation.check()?;
                    let line = raw_line.trim_end_matches('\r');
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let item_path = PathBuf::from(line);
                    let item_path = logical_path(&if item_path.is_absolute() {
                        item_path
                    } else {
                        parent.join(item_path)
                    })?;
                    if let Some(roots) = &roots {
                        ensure!(
                            roots.iter().any(|root| item_path.starts_with(root)),
                            "Import source is outside configured library roots: {}",
                            item_path.display()
                        );
                    }
                    ensure!(
                        paths.len() < MAX_IMPORT_ENTRIES,
                        "import expansion exceeds {MAX_IMPORT_ENTRIES} entries"
                    );
                    retained += std::mem::size_of::<PathBuf>() + item_path.as_os_str().len();
                    ensure!(
                        retained <= MAX_RETAINED_BYTES,
                        "import expansion exceeds retained data limit"
                    );
                    paths.push(item_path);
                }
                Ok(paths)
            })
            .await
            .context("Playlist import parser panicked")??
    };
    for (index, path) in paths.into_iter().enumerate() {
        cancellation.check()?;
        if is_cue_path(&path) {
            if let Some(roots) = roots {
                ensure!(
                    roots.iter().any(|root| path.starts_with(root)),
                    "CUE sheet is outside configured library roots: {}",
                    path.display()
                );
            }
            let bytes = if builder.result.sheets.contains_key(&path) {
                None
            } else {
                let bytes = read_input_async(
                    workers,
                    &path,
                    MAX_CUE_BYTES.min(MAX_IMPORT_BYTES - builder.bytes_read),
                    cancellation,
                )
                .await
                .with_context(|| format!("Reading import CUE {}", path.display()))?;
                builder.bytes_read += bytes.len();
                Some(bytes)
            };
            let cancellation = cancellation.clone();
            builder = workers
                .execute(move || -> Result<ImportBuilder> {
                    let sheet = bytes
                        .map(|bytes| super::cue::parse_input(&bytes, &path, &cancellation))
                        .transpose()?;
                    builder.add_sheet(path, sheet, &cancellation)?;
                    Ok(builder)
                })
                .await
                .context("Playlist import CUE parser panicked")??;
        } else {
            ensure!(
                builder.result.items.len() < MAX_IMPORT_ENTRIES,
                "import expansion exceeds {MAX_IMPORT_ENTRIES} entries"
            );
            builder.expanded_bytes += std::mem::size_of::<M3uItem>() + path.as_os_str().len();
            ensure!(
                builder.expanded_bytes <= MAX_RETAINED_BYTES,
                "import expansion exceeds retained data limit"
            );
            builder.result.items.push(M3uItem {
                path,
                cue_track: None,
            });
        }
        if index % COOPERATIVE_ROWS == COOPERATIVE_ROWS - 1 {
            futures_lite::future::yield_now().await;
        }
    }
    cancellation.check()?;
    ensure!(
        !builder.result.items.is_empty(),
        "Import playlist contains no entries"
    );
    Ok(builder.result)
}

/// Export a database-ordered stream of unique playlist tracks atomically.
/// SQLite enforces uniqueness; retain only the current CUE validation run.
pub async fn export_m3u<S>(
    workers: &BlockingPool,
    path: &Path,
    tracks: S,
    cancellation: &ScanCancellation,
) -> Result<()>
where
    S: Stream<Item = Result<PlaylistExportTrack>>,
{
    cancellation.check()?;
    let path = logical_path(path)?;
    let parent = path
        .parent()
        .context("playlist has no parent directory")?
        .to_path_buf();
    let temporary_parent = parent.clone();
    let (file, temporary) = workers
        .execute(move || {
            tempfile::NamedTempFile::new_in(temporary_parent)
                .map(tempfile::NamedTempFile::into_parts)
                .context("create temporary playlist")
        })
        .await
        .context("Playlist temporary file worker panicked")??;
    let mut writer = ExportWriter {
        workers,
        file: Some(file),
        bytes: Vec::with_capacity(EXPORT_BUFFER_BYTES),
        cancellation: cancellation.clone(),
    };
    let result = std::panic::AssertUnwindSafe(write_export(
        workers,
        &mut writer,
        &parent,
        tracks,
        cancellation,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|_| {
        Err(anyhow!(
            "Playlist export writer panicked; destination was not replaced"
        ))
    });
    // Every submitted write has physically ended before closing or replacing.
    let file = writer.file.take();
    let cancellation = cancellation.clone();
    workers
        .execute(move || -> Result<()> {
            drop(file);
            result?;
            // Atomic file commit: cancellation after this check cannot undo replacement.
            cancellation.check()?;
            temporary
                .persist(&path)
                .with_context(|| format!("replace playlist {}", path.display()))?;
            Ok(())
        })
        .await
        .context("Playlist replacement worker panicked")?
}

async fn write_export<S>(
    workers: &BlockingPool,
    writer: &mut ExportWriter<'_>,
    parent: &Path,
    tracks: S,
    cancellation: &ScanCancellation,
) -> Result<()>
where
    S: Stream<Item = Result<PlaylistExportTrack>>,
{
    writer.write_all(b"#EXTM3U\n").await?;
    futures::pin_mut!(tracks);
    let mut cue_run: Option<(PathBuf, super::cue::CueSheet, usize)> = None;
    let mut rows = 0;
    while let Some(item) = tracks.next().await {
        cancellation.check()?;
        // Count continuations too: a ready CUE run must not monopolize polling.
        rows += 1;
        if rows == COOPERATIVE_ROWS {
            rows = 0;
            futures_lite::future::yield_now().await;
            cancellation.check()?;
        }
        let mut track = item?;
        track.path = logical_path(&track.path)?;
        if let Some(cue) = &mut track.cue {
            cue.sheet = logical_path(&cue.sheet)?;
        }
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
            if *index == sheet.tracks.len() {
                cue_run = None;
            }
            continue;
        }
        if let Some(cue) = &track.cue {
            let bytes = read_input_async(workers, &cue.sheet, MAX_CUE_BYTES, cancellation)
                .await
                .with_context(|| format!("Reading export CUE {}", cue.sheet.display()))?;
            let sheet_path = cue.sheet.clone();
            let parse_cancellation = cancellation.clone();
            let sheet = workers
                .execute(move || super::cue::parse_input(&bytes, &sheet_path, &parse_cancellation))
                .await
                .context("Playlist export CUE parser panicked")??;
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
            write_relative_path(writer, parent, &cue.sheet).await?;
            if sheet.tracks.len() > 1 {
                cue_run = Some((cue.sheet.clone(), sheet, 1));
            }
            continue;
        }
        let duration = track
            .duration
            .map(|seconds| seconds.max(0.0).round() as i64)
            .unwrap_or(-1);
        let mut header = std::io::Cursor::new([0u8; 32]);
        write!(&mut header, "#EXTINF:{duration},")?;
        writer
            .write_all(&header.get_ref()[..header.position() as usize])
            .await?;
        if !track.artist.is_empty() {
            write_single_line(writer, &track.artist).await?;
            if !track.title.is_empty() {
                writer.write_all(b" - ").await?;
            }
        }
        write_single_line(writer, &track.title).await?;
        writer.write_all(b"\n").await?;
        write_relative_path(writer, parent, &track.path).await?;
    }
    if let Some((sheet, _, _)) = cue_run {
        bail!(
            "Standard M3U cannot represent partial or reordered CUE tracks; include every track from {} in sheet order",
            sheet.display()
        );
    }
    writer.flush().await?;
    Ok(())
}

async fn write_single_line(writer: &mut ExportWriter<'_>, text: &str) -> Result<()> {
    for (index, part) in text.split(['\r', '\n']).enumerate() {
        if index != 0 {
            writer.write_all(b" ").await?;
        }
        writer.write_all(part.as_bytes()).await?;
    }
    Ok(())
}

async fn write_relative_path(
    writer: &mut ExportWriter<'_>,
    parent: &Path,
    track: &Path,
) -> Result<()> {
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
        writer.write_all(b"./").await?;
    }
    writer.write_all(text.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

fn path_relative_to(parent: &Path, target: &Path) -> PathBuf {
    let mut parent_components = parent.components().peekable();
    let mut target_components = target.components().peekable();
    while parent_components.peek().is_some() && parent_components.peek() == target_components.peek()
    {
        parent_components.next();
        target_components.next();
    }
    let mut relative = PathBuf::new();
    for component in parent_components {
        if !matches!(component, std::path::Component::Prefix(_)) {
            relative.push("..");
        }
    }
    for component in target_components {
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

    #[test]
    fn stream_panic_preserves_destination_and_retires_temporary_file() -> Result<()> {
        futures::executor::block_on(async {
            let workers = &BlockingPool::new(4);
            let directory = tempfile::tempdir()?;
            let destination = directory.path().join("playlist.m3u8");
            let original = b"original playlist\n";
            std::fs::write(&destination, original)?;
            let track = PlaylistExportTrack {
                path: directory.path().join("song.wav"),
                title: "x".repeat(EXPORT_BUFFER_BYTES * 2),
                artist: String::new(),
                duration: None,
                cue: None,
            };
            let tracks = std::iter::once(Ok(track)).chain(std::iter::from_fn(|| {
                panic!("injected export stream panic")
            }));
            let error = export_m3u(
                workers,
                &destination,
                futures::stream::iter(tracks),
                &ScanCancellation::default(),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("writer panicked"));
            assert_eq!(std::fs::read(&destination)?, original);
            assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
            Ok(())
        })
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn ready_rows_yield_in_bounded_batches_and_cancel_without_replacement() -> Result<()> {
        let executor = async_executor::Executor::new();
        futures::executor::block_on(executor.run(async {
            let workers = &BlockingPool::new(4);
            let directory = tempfile::tempdir()?;
            let destination = directory.path().join("playlist.m3u8");
            std::fs::write(&destination, b"original\n")?;
            let cancellation = ScanCancellation::default();
            let reads = Arc::new(AtomicUsize::new(0));
            let observed = Arc::new(AtomicUsize::new(0));
            let track = PlaylistExportTrack {
                path: directory.path().join("song.wav"),
                title: "Song".into(),
                artist: String::new(),
                duration: None,
                cue: None,
            };
            let tracks = futures::stream::iter((0..COOPERATIVE_ROWS * 2).map(|index| {
                reads.fetch_add(1, Ordering::Relaxed);
                if index == 0 {
                    let reads = reads.clone();
                    let observed = observed.clone();
                    let cancellation = cancellation.clone();
                    executor
                        .spawn(async move {
                            observed.store(reads.load(Ordering::Relaxed), Ordering::Relaxed);
                            cancellation.cancel();
                        })
                        .detach();
                }
                Ok(track.clone())
            }));
            let error = export_m3u(workers, &destination, tracks, &cancellation)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("cancelled"));
            assert_eq!(observed.load(Ordering::Relaxed), COOPERATIVE_ROWS);
            assert_eq!(std::fs::read(&destination)?, b"original\n");
            assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
            Ok(())
        }))
    }
}
