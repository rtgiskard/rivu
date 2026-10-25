use super::paths::{is_audio_path, is_cue_path, logical_path};
use super::source::{HASH_CHUNK_BYTES, KnownFile, SOURCE_BATCH_SIZE, ScanRecord, prepare_sources};
use crate::core::tasks::BlockingPool;
use anyhow::{Context, Result, anyhow, bail};
use rivu_protocol::model::{CueSegment, ScanPhase};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation shared by the scan owner and its async preparation.
#[derive(Clone, Debug, Default)]
pub struct ScanCancellation {
    state: Arc<ScanCancellationState>,
}

#[derive(Debug, Default)]
struct ScanCancellationState {
    cancelled: AtomicBool,
    event: event_listener::Event,
}

impl ScanCancellation {
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.event.notify(usize::MAX);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        while !self.is_cancelled() {
            let listener = self.state.event.listen();
            // Register before checking again so cancellation cannot be missed.
            if self.is_cancelled() {
                return;
            }
            listener.await;
        }
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("scan cancelled");
        }
        Ok(())
    }
}

pub(super) const MAX_REPORTED_SCAN_ERRORS: usize = 256;
pub(super) const MAX_RECORDED_SKIPPED_DIRECTORIES: usize = 256;
pub(super) const MAX_RECORDED_SCAN_UNCERTAINTIES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedDirectory {
    /// Index of the requested root in `ScanResult::roots` that excluded this path.
    pub root_index: usize,
    pub path: PathBuf,
}

/// An area whose absence could not be established by this scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanUncertainty {
    /// Metadata for a requested root failed for a reason other than NotFound.
    Root { root_index: usize },
    /// Traversal failed within this requested root, including transient NotFound.
    Subtree { root_index: usize, path: PathBuf },
    /// A whole source could not be read, probed or hashed.
    File(PathBuf),
    /// A sheet failed atomically; none of its track identities were established.
    CueSheet(PathBuf),
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    /// Normalized, absolute UTF-8 scopes accepted for reconciliation coverage.
    pub roots: Vec<PathBuf>,
    pub records: Vec<ScanRecord>,
    /// Admission threshold captured when the scan starts; zero disables filtering.
    /// Keep all probed records here so storage can resolve existing identities first.
    pub min_duration_secs: u32,
    pub errors: Vec<String>,
    /// Directories excluded because they exceed the configured scan depth.
    pub skipped_directories: Vec<SkippedDirectory>,
    /// Additional skipped-directory paths were omitted after the memory bound.
    pub skipped_directories_truncated: bool,
    /// Failed filesystem, source and sheet scopes; bounded independently of diagnostics.
    pub uncertainties: Vec<ScanUncertainty>,
    /// Bounded metadata omitted details: preserve absent identities and disable relocation.
    pub scan_incomplete: bool,
    /// Whole sources represented by successfully scanned CUE sheets.
    pub suppressed_sources: Vec<PathBuf>,
}

impl ScanResult {
    fn push_error(&mut self, error: impl FnOnce() -> String) {
        if self.errors.len() < MAX_REPORTED_SCAN_ERRORS - 1 {
            self.errors.push(error());
        } else if self.errors.len() == MAX_REPORTED_SCAN_ERRORS - 1 {
            self.errors.push(format!(
                "additional scan errors omitted after {} entries",
                MAX_REPORTED_SCAN_ERRORS - 1
            ));
            self.scan_incomplete = true;
        }
    }

    fn push_skipped_directory(&mut self, root_index: usize, path: &Path) -> bool {
        if self.skipped_directories_truncated {
            return false;
        }
        if self.skipped_directories.iter().any(|entry| {
            self.roots[entry.root_index] == self.roots[root_index] && entry.path == path
        }) {
            return false;
        }
        if self.skipped_directories.len() == MAX_RECORDED_SKIPPED_DIRECTORIES {
            self.skipped_directories_truncated = true;
            self.scan_incomplete = true;
            return false;
        }
        self.skipped_directories.push(SkippedDirectory {
            root_index,
            path: path.to_path_buf(),
        });
        true
    }

    fn push_uncertainty(&mut self, uncertainty: ScanUncertainty) {
        if self.uncertainties.contains(&uncertainty) {
            return;
        }
        if self.uncertainties.len() == MAX_RECORDED_SCAN_UNCERTAINTIES {
            self.scan_incomplete = true;
            return;
        }
        self.uncertainties.push(uncertainty);
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn summary(&self, skipped_short_tracks: usize) -> String {
        let records = self.records.len().saturating_sub(skipped_short_tracks);
        match (skipped_short_tracks, self.errors.len()) {
            (0, 0) => format!("Found {records} audio files"),
            (0, errors) => format!("Found {records} audio files ({errors} errors)"),
            (skipped, 0) => format!("Found {records} audio files ({skipped} short tracks skipped)"),
            (skipped, errors) => format!(
                "Found {records} audio files ({skipped} short tracks skipped, {errors} errors)"
            ),
        }
    }

    /// Unknown, non-finite and non-positive durations do not establish shortness.
    pub(crate) fn excludes_new_track(&self, duration: Option<f64>) -> bool {
        self.min_duration_secs != 0
            && duration.is_some_and(|duration| {
                duration.is_finite()
                    && duration > 0.0
                    && duration < f64::from(self.min_duration_secs)
            })
    }
}

/// Recheck only the supplied physical sources, without traversing directories or
/// expanding playlists/CUE sheets. Roots record confirmed absence only; failed
/// opens, probes and permission checks never authorize missing reconciliation.
pub(crate) async fn recheck_sources<Fut>(
    workers: &BlockingPool,
    paths: &[PathBuf],
    mut known_sources: impl FnMut(Vec<PathBuf>) -> Fut,
    ffmpeg_enabled: bool,
    cancellation: &ScanCancellation,
    mut progress: impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) -> Result<ScanResult>
where
    Fut: Future<Output = Result<Vec<Option<KnownFile>>>>,
{
    cancellation.check()?;
    let mut paths = paths
        .iter()
        .map(|path| logical_path(path))
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    paths.dedup();
    let total = paths.len();
    let mut result = ScanResult::default();
    let mut scratch = Vec::new();
    let mut processed = 0;
    progress(ScanPhase::ReadingMetadata, 0, Some(total), 0, None);
    for chunk in paths.chunks(SOURCE_BATCH_SIZE) {
        cancellation.check()?;
        let batch = chunk.to_vec();
        let token = cancellation.clone();
        let inspected = workers
            .execute(move || {
                let mut inspected = Vec::with_capacity(batch.len());
                for path in batch {
                    token.check()?;
                    path.to_str().context("Audio source path is not UTF-8")?;
                    let metadata = fs::metadata(&path);
                    inspected.push((path, metadata));
                }
                Ok::<_, anyhow::Error>(inspected)
            })
            .await
            .context("Registered source inspection worker failed")??;
        cancellation.check()?;
        let mut present = Vec::with_capacity(inspected.len());
        for (path, metadata) in inspected {
            match metadata {
                Ok(_) => present.push(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    result.roots.push(path);
                }
                Err(error) => result.push_error(|| format!("{}: {error}", path.display())),
            }
        }
        if !present.is_empty() && scratch.is_empty() {
            scratch.resize(HASH_CHUNK_BYTES, 0);
        }
        let mut sources = HashMap::new();
        prepare_sources(
            workers,
            present,
            &mut known_sources,
            &mut sources,
            ffmpeg_enabled,
            cancellation,
            &mut scratch,
        )
        .await?;
        for path in chunk {
            cancellation.check()?;
            if let Some(source) = sources.remove(path) {
                match source {
                    Ok(record) => result.records.push(record),
                    Err(error) => result.push_error(|| format!("{}: {error}", path.display())),
                }
            }
            processed += 1;
            progress(
                ScanPhase::ReadingMetadata,
                processed,
                Some(total),
                result.errors.len(),
                Some(path),
            );
        }
    }
    cancellation.check()?;
    Ok(result)
}

/// Scan directories, audio files and CUE sheets without touching audio output.
/// CUE metadata is always reread; only unchanged full-source probe results may
/// be reused. Failed sheets contribute no partial tracks or source suppression.
/// `known_sources` looks up only the admitted sources in a bounded batch.
/// `library_roots` are normalized absolute logical paths, as published by the core.
/// `max_depth` is relative to each requested directory, which is itself depth 0.
/// Cancellation returns an error, never a partial result. Checks surround blocking
/// probes and lookups, but an in-flight probe itself is not interruptible.
#[cfg(test)]
pub fn scan_paths(
    paths: &[PathBuf],
    mut known_sources: impl FnMut(&[PathBuf]) -> Result<Vec<Option<KnownFile>>>,
    ffmpeg_enabled: bool,
    library_roots: &[PathBuf],
    max_depth: u32,
    cancellation: &ScanCancellation,
    progress: impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) -> Result<ScanResult> {
    let workers = &BlockingPool::new(4);
    futures::executor::block_on(scan_paths_with_cues(
        workers,
        paths,
        |paths| std::future::ready(known_sources(&paths)),
        ScanSettings {
            ffmpeg_enabled,
            library_roots,
            max_depth,
        },
        (cancellation, None),
        progress,
    ))
}

pub(crate) struct ScanSettings<'a> {
    pub(crate) ffmpeg_enabled: bool,
    /// Normalized absolute logical paths published by the core.
    pub(crate) library_roots: &'a [PathBuf],
    /// Depth relative to each requested directory, which is depth zero.
    pub(crate) max_depth: u32,
}

pub(crate) async fn scan_paths_with_cues<Fut>(
    workers: &BlockingPool,
    paths: &[PathBuf],
    mut known_sources: impl FnMut(Vec<PathBuf>) -> Fut,
    ScanSettings {
        ffmpeg_enabled,
        library_roots,
        max_depth,
    }: ScanSettings<'_>,
    (cancellation, mut sheets): (
        &ScanCancellation,
        Option<HashMap<PathBuf, super::cue::CueSheet>>,
    ),
    mut progress: impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) -> Result<ScanResult>
where
    Fut: Future<Output = Result<Vec<Option<KnownFile>>>>,
{
    cancellation.check()?;
    if max_depth == 0 {
        bail!("scan max depth must be at least 1");
    }
    let mut files = HashSet::new();
    let mut explicit_audio = HashSet::new();
    let mut result = ScanResult::default();
    let mut seen_requests = HashSet::with_capacity(paths.len());
    for requested in paths {
        cancellation.check()?;
        let root = logical_path(requested)?;
        if !seen_requests.insert(root.clone()) {
            continue;
        }
        // Only persistable scopes may authorize reconciliation. Descendants of a
        // valid scope are still handled as local source/sheet errors below.
        if root.to_str().is_none() {
            result.push_error(|| format!("{}: Scan scope path is not UTF-8", root.display()));
            progress(
                ScanPhase::Discovering,
                files.len(),
                None,
                result.errors.len(),
                Some(root.as_path()),
            );
            continue;
        }
        if !library_roots
            .iter()
            .any(|allowed| root.starts_with(allowed))
        {
            result.push_error(|| {
                format!("{} is outside the configured library roots", root.display())
            });
            progress(
                ScanPhase::Discovering,
                files.len(),
                None,
                result.errors.len(),
                Some(root.as_path()),
            );
            continue;
        }
        result.roots.push(root.clone());
        let root_index = result.roots.len() - 1;
        let metadata = match workers
            .execute({
                let root = root.clone();
                move || fs::metadata(root)
            })
            .await
            .context("Scan root metadata worker failed")?
        {
            Ok(metadata) => metadata,
            Err(error) => {
                cancellation.check()?;
                if error.kind() != std::io::ErrorKind::NotFound {
                    result.push_uncertainty(ScanUncertainty::Root { root_index });
                }
                result.push_error(|| format!("{}: {error}", root.display()));
                progress(
                    ScanPhase::Discovering,
                    files.len(),
                    None,
                    result.errors.len(),
                    Some(root.as_path()),
                );
                continue;
            }
        };
        cancellation.check()?;
        if !metadata.is_dir() {
            if is_audio_path(&root) {
                explicit_audio.insert(root.clone());
                if files.insert(root.clone()) {
                    progress(
                        ScanPhase::Discovering,
                        files.len(),
                        None,
                        result.errors.len(),
                        Some(root.as_path()),
                    );
                }
            } else if is_cue_path(&root) {
                if files.insert(root.clone()) {
                    progress(
                        ScanPhase::Discovering,
                        files.len(),
                        None,
                        result.errors.len(),
                        Some(root.as_path()),
                    );
                }
            } else {
                result.push_error(|| {
                    format!(
                        "{} is not a supported audio file or CUE sheet",
                        root.display()
                    )
                });
                progress(
                    ScanPhase::Discovering,
                    files.len(),
                    None,
                    result.errors.len(),
                    Some(root.as_path()),
                );
            }
        } else {
            discover_directory(
                workers,
                &root,
                metadata,
                (root_index, max_depth),
                cancellation,
                (&mut files, &mut result),
                &mut progress,
            )
            .await?;
        }
    }
    cancellation.check()?;
    progress(
        ScanPhase::Discovering,
        files.len(),
        None,
        result.errors.len(),
        None,
    );
    let files = workers
        .execute(move || {
            let mut files: Vec<_> = files.into_iter().collect();
            files.sort();
            files
        })
        .await
        .context("Scan discovery ordering worker failed")?;
    cancellation.check()?;
    let total = files.len();
    let mut processed = 0;
    progress(
        ScanPhase::ReadingMetadata,
        processed,
        Some(total),
        result.errors.len(),
        None,
    );
    let mut sources = HashMap::new();
    let mut suppressed = HashSet::new();
    let mut scratch = vec![0; HASH_CHUNK_BYTES];
    let mut cue_paths = files.iter().filter(|path| is_cue_path(path));
    loop {
        let mut prepared = Vec::new();
        let mut pending = HashSet::new();
        for path in cue_paths.by_ref().take(SOURCE_BATCH_SIZE) {
            cancellation.check()?;
            let sheet = read_cue(workers, path, library_roots, cancellation, sheets.as_mut()).await;
            if let Ok(sheet) = &sheet {
                for track in &sheet.tracks {
                    if !sources.contains_key(&track.file) {
                        pending.insert(track.file.clone());
                    }
                }
            }
            prepared.push((path, sheet));
        }
        if prepared.is_empty() {
            break;
        }
        let mut pending: Vec<_> = pending.into_iter().collect();
        pending.sort();
        prepare_sources(
            workers,
            pending,
            &mut known_sources,
            &mut sources,
            ffmpeg_enabled,
            cancellation,
            &mut scratch,
        )
        .await?;
        for (path, sheet) in prepared {
            cancellation.check()?;
            match sheet.and_then(|sheet| cue_records(path, sheet, &sources, cancellation)) {
                Ok(records) => {
                    suppressed.extend(records.iter().map(|record| record.path.clone()));
                    result.records.extend(records);
                }
                Err(error) => {
                    cancellation.check()?;
                    result.push_uncertainty(ScanUncertainty::CueSheet(path.clone()));
                    result.push_error(|| format!("{}: {error:#}", path.display()));
                }
            }
            processed += 1;
            progress(
                ScanPhase::ReadingMetadata,
                processed,
                Some(total),
                result.errors.len(),
                Some(path.as_path()),
            );
        }
    }
    for (path, source) in &sources {
        if source.is_err() {
            result.push_uncertainty(ScanUncertainty::File(path.clone()));
        }
    }
    for chunk in files.chunks(SOURCE_BATCH_SIZE) {
        let pending = chunk
            .iter()
            .filter(|path| {
                is_audio_path(path)
                    && (!suppressed.contains(*path) || explicit_audio.contains(*path))
                    && !sources.contains_key(*path)
            })
            .cloned()
            .collect();
        prepare_sources(
            workers,
            pending,
            &mut known_sources,
            &mut sources,
            ffmpeg_enabled,
            cancellation,
            &mut scratch,
        )
        .await?;
        for path in chunk.iter().filter(|path| is_audio_path(path)) {
            cancellation.check()?;
            if !suppressed.contains(path) || explicit_audio.contains(path) {
                let record = sources
                    .remove(path)
                    .context("Audio source was not prepared")?;
                match record {
                    Ok(record) => result.records.push(record),
                    Err(error) => {
                        result.push_uncertainty(ScanUncertainty::File(path.clone()));
                        result.push_error(|| format!("{}: {error}", path.display()));
                    }
                }
            }
            processed += 1;
            progress(
                ScanPhase::ReadingMetadata,
                processed,
                Some(total),
                result.errors.len(),
                Some(path.as_path()),
            );
        }
    }
    cancellation.check()?;
    let result = workers
        .execute(move || {
            suppressed.retain(|path| !explicit_audio.contains(path));
            result.suppressed_sources = suppressed.into_iter().collect();
            result.suppressed_sources.sort();
            result.skipped_directories.sort_unstable_by(|a, b| {
                a.path
                    .cmp(&b.path)
                    .then_with(|| a.root_index.cmp(&b.root_index))
            });
            result.records.sort_by(|a, b| {
                let identity =
                    |record: &ScanRecord| record.cue.as_ref().map(|cue| cue.number).unwrap_or(0);
                let a_path = a.cue.as_ref().map_or(&a.path, |cue| &cue.sheet);
                let b_path = b.cue.as_ref().map_or(&b.path, |cue| &cue.sheet);
                a_path
                    .cmp(b_path)
                    .then_with(|| identity(a).cmp(&identity(b)))
            });
            result
        })
        .await
        .context("Scan result ordering worker failed")?;
    progress(
        ScanPhase::ReadingMetadata,
        processed,
        Some(total),
        result.errors.len(),
        None,
    );
    cancellation.check()?;
    Ok(result)
}

/// One sequential DFS: at most max_depth + 1 directory readers, no task per
/// entry, and inode ancestry detects loops without erasing logical aliases.
async fn discover_directory(
    workers: &BlockingPool,
    root: &Path,
    metadata: fs::Metadata,
    (root_index, max_depth): (usize, u32),
    cancellation: &ScanCancellation,
    (files, result): (&mut HashSet<PathBuf>, &mut ScanResult),
    progress: &mut impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    struct Directory {
        path: PathBuf,
        identity: (u64, u64),
        entries: fs::ReadDir,
    }
    enum Discovery {
        File(PathBuf),
        Error(PathBuf, std::io::Error),
        Depth(PathBuf),
    }

    let root_path = root.to_path_buf();
    let entries = workers
        .execute(move || fs::read_dir(root_path))
        .await
        .context("Scan directory admission worker failed")?;
    let entries = match entries {
        Ok(entries) => entries,
        Err(error) => {
            traversal_error(root_index, root, error, files.len(), result, progress);
            cancellation.check()?;
            return Ok(());
        }
    };
    let mut stack = vec![Directory {
        path: root.to_path_buf(),
        identity: (metadata.dev(), metadata.ino()),
        entries,
    }];
    while !stack.is_empty() {
        cancellation.check()?;
        let token = cancellation.clone();
        let (next_stack, discoveries) = workers
            .execute(move || {
                let mut discoveries = Vec::new();
                // Count reader exhaustion/errors too, so a deep unwind is bounded.
                for _ in 0..DIRECTORY_BATCH_SIZE {
                    token.check()?;
                    let Some(directory) = stack.last_mut() else {
                        break;
                    };
                    let entry = match directory.entries.next() {
                        Some(Ok(entry)) => entry,
                        None => {
                            stack.pop();
                            continue;
                        }
                        Some(Err(error)) => {
                            let path = directory.path.clone();
                            stack.pop();
                            discoveries.push(Discovery::Error(path, error));
                            continue;
                        }
                    };
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(file_type) => file_type,
                        Err(error) => {
                            discoveries.push(Discovery::Error(path, error));
                            continue;
                        }
                    };
                    let metadata = if file_type.is_dir() || file_type.is_symlink() {
                        match fs::metadata(&path) {
                            Ok(metadata) => Some(metadata),
                            Err(error) => {
                                discoveries.push(Discovery::Error(path, error));
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    token.check()?;
                    if let Some(metadata) = metadata.filter(fs::Metadata::is_dir) {
                        if stack.len() > max_depth as usize {
                            discoveries.push(Discovery::Depth(path));
                            continue;
                        }
                        let identity = (metadata.dev(), metadata.ino());
                        if stack.iter().any(|directory| directory.identity == identity) {
                            discoveries.push(Discovery::Error(
                                path,
                                std::io::Error::other("filesystem loop detected"),
                            ));
                            continue;
                        }
                        match fs::read_dir(&path) {
                            Ok(entries) => stack.push(Directory {
                                path,
                                identity,
                                entries,
                            }),
                            Err(error) => discoveries.push(Discovery::Error(path, error)),
                        }
                    } else if is_audio_path(&path) || is_cue_path(&path) {
                        discoveries.push(Discovery::File(path));
                    }
                }
                token.check()?;
                Ok::<_, anyhow::Error>((stack, discoveries))
            })
            .await
            .context("Scan directory batch worker failed")??;
        stack = next_stack;
        cancellation.check()?;
        for discovery in discoveries {
            match discovery {
                Discovery::Error(path, error) => {
                    traversal_error(root_index, &path, error, files.len(), result, progress);
                }
                Discovery::Depth(path) => {
                    if result.push_skipped_directory(root_index, &path) {
                        tracing::warn!(path = %path.display(), max_depth, "scan_depth_limit_reached");
                    }
                    result.push_error(|| {
                        format!(
                            "{}: directory exceeds scan depth limit {max_depth}",
                            path.display()
                        )
                    });
                    progress(
                        ScanPhase::Discovering,
                        files.len(),
                        None,
                        result.errors.len(),
                        Some(&path),
                    );
                }
                Discovery::File(path) => {
                    // This type hint never admits the actual object for reading.
                    if files.insert(path.clone()) {
                        progress(
                            ScanPhase::Discovering,
                            files.len(),
                            None,
                            result.errors.len(),
                            Some(&path),
                        );
                    }
                }
            }
        }
    }
    cancellation.check()
}

fn traversal_error(
    root_index: usize,
    path: &Path,
    error: std::io::Error,
    processed: usize,
    result: &mut ScanResult,
    progress: &mut impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) {
    result.push_uncertainty(ScanUncertainty::Subtree {
        root_index,
        path: path.to_path_buf(),
    });
    result.push_error(|| format!("{}: {error}", path.display()));
    progress(
        ScanPhase::Discovering,
        processed,
        None,
        result.errors.len(),
        Some(path),
    );
}

async fn read_cue(
    workers: &BlockingPool,
    path: &Path,
    library_roots: &[PathBuf],
    cancellation: &ScanCancellation,
    sheets: Option<&mut HashMap<PathBuf, super::cue::CueSheet>>,
) -> Result<super::cue::CueSheet> {
    cancellation.check()?;
    path.to_str().context("CUE sheet path is not UTF-8")?;
    let sheet = match sheets {
        Some(sheets) => sheets
            .remove(path)
            .context("Import CUE sheet was not prepared"),
        None => {
            let bytes = super::input::read_input_async(
                workers,
                path,
                super::input::MAX_CUE_BYTES,
                cancellation,
            )
            .await
            .with_context(|| format!("Reading CUE {}", path.display()))?;
            let path = path.to_path_buf();
            let cancellation = cancellation.clone();
            workers
                .execute(move || super::cue::parse_input(&bytes, &path, &cancellation))
                .await
                .context("CUE parser worker failed")?
        }
    };
    cancellation.check()?;
    let mut sheet = sheet?;
    for track in &mut sheet.tracks {
        cancellation.check()?;
        track.file = logical_path(&track.file)
            .with_context(|| format!("resolve CUE source {}", track.file.display()))?;
        if !library_roots
            .iter()
            .any(|root| track.file.starts_with(root))
        {
            bail!(
                "CUE source {} is outside the configured library roots",
                track.file.display()
            );
        }
    }
    Ok(sheet)
}

fn cue_records(
    path: &Path,
    sheet: super::cue::CueSheet,
    sources: &HashMap<PathBuf, std::result::Result<ScanRecord, String>>,
    cancellation: &ScanCancellation,
) -> Result<Vec<ScanRecord>> {
    let mut records = Vec::with_capacity(sheet.tracks.len());
    for track in sheet.tracks {
        cancellation.check()?;
        let source = sources
            .get(&track.file)
            .context("CUE source was not prepared")?
            .as_ref()
            .map_err(|error| anyhow!("{error}"))?;
        let duration = source
            .media
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .with_context(|| {
                format!(
                    "CUE source {} has no finite duration",
                    source.path.display()
                )
            })?;
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
        let mut record = source.clone();
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
    cancellation.check()?;
    Ok(records)
}

const DIRECTORY_BATCH_SIZE: usize = 64;

#[cfg(test)]
mod recheck_tests {
    use super::super::source::cache_modified_ns;
    use super::*;
    use rivu_audio::MediaInfo;

    fn cached(path: &Path) -> KnownFile {
        let metadata = fs::metadata(path).unwrap();
        KnownFile {
            size: metadata.len(),
            modified_ns: cache_modified_ns(metadata.modified().ok()),
            fingerprint: Some(blake3::hash(b"cached source").to_hex().to_string()),
            cue: None,
            media: Some(MediaInfo {
                title: "Cached".into(),
                artist: String::new(),
                album: String::new(),
                duration: Some(1.0),
                codec: "pcm".into(),
                channels: 2,
                sample_rate: 48000,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            }),
        }
    }

    #[test]
    fn exact_recheck_never_expands_inputs_and_only_confirms_absence() {
        futures::executor::block_on(async {
            let workers = &BlockingPool::new(4);
            let directory = tempfile::tempdir().unwrap();
            let nested = directory.path().join("nested");
            fs::create_dir(&nested).unwrap();
            fs::write(nested.join("unregistered.wav"), b"audio not requested").unwrap();
            let sheet = directory.path().join("registered.cue");
            fs::write(
                &sheet,
                b"FILE \"nested/unregistered.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
            )
            .unwrap();
            let missing = directory.path().join("absent.wav");
            let invalid = directory.path().join("invalid.wav");
            fs::write(&invalid, b"not audio").unwrap();
            let mut looked_up = Vec::new();
            let result = recheck_sources(
                workers,
                &[
                    nested.clone(),
                    sheet.clone(),
                    missing.clone(),
                    invalid.clone(),
                ],
                |paths| {
                    looked_up.extend(paths.iter().cloned());
                    std::future::ready(Ok(vec![None; paths.len()]))
                },
                false,
                &ScanCancellation::default(),
                |_, _, _, _, _| {},
            )
            .await
            .unwrap();
            assert_eq!(result.roots, vec![missing]);
            assert!(result.records.is_empty());
            assert_eq!(result.errors.len(), 3);
            assert!(
                looked_up
                    .iter()
                    .all(|path| path == &sheet || path == &invalid)
            );
            assert!(!looked_up.contains(&nested.join("unregistered.wav")));
        })
    }

    #[test]
    fn exact_recheck_reuses_cache_and_force_callback_bypasses_it() {
        futures::executor::block_on(async {
            let workers = &BlockingPool::new(4);
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("cached.wav");
            fs::write(&path, b"intentionally unprobeable cached bytes").unwrap();
            let known = cached(&path);
            let result = recheck_sources(
                workers,
                &[path.clone(), path.clone()],
                |paths| std::future::ready(Ok(vec![Some(known.clone()); paths.len()])),
                false,
                &ScanCancellation::default(),
                |_, _, _, _, _| {},
            )
            .await
            .unwrap();
            assert_eq!(result.records.len(), 1);
            assert_eq!(result.records[0].media.title, "Cached");
            assert!(result.errors.is_empty());
            let forced = recheck_sources(
                workers,
                &[path],
                |paths| std::future::ready(Ok(vec![None; paths.len()])),
                false,
                &ScanCancellation::default(),
                |_, _, _, _, _| {},
            )
            .await
            .unwrap();
            assert!(forced.records.is_empty());
            assert_eq!(forced.errors.len(), 1);
            assert!(forced.roots.is_empty());
        })
    }

    #[test]
    fn exact_recheck_preserves_probe_failures_and_observes_cancellation_after_lookup() {
        futures::executor::block_on(async {
            let workers = &BlockingPool::new(4);
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("probe.wav");
            fs::write(&path, b"unreadable probe").unwrap();
            let failed = recheck_sources(
                workers,
                std::slice::from_ref(&path),
                |_| std::future::ready(Err(anyhow!(std::io::ErrorKind::PermissionDenied))),
                false,
                &ScanCancellation::default(),
                |_, _, _, _, _| {},
            )
            .await
            .unwrap();
            assert!(failed.records.is_empty());
            assert!(failed.roots.is_empty());
            assert_eq!(failed.errors.len(), 1);
            let cancellation = ScanCancellation::default();
            let cancelled = recheck_sources(
                workers,
                &[path],
                |paths| {
                    cancellation.cancel();
                    std::future::ready(Ok(vec![None; paths.len()]))
                },
                false,
                &cancellation,
                |_, _, _, _, _| {},
            )
            .await;
            assert!(cancelled.unwrap_err().to_string().contains("cancelled"));
        })
    }
}
