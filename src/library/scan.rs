use super::paths::{is_audio_path, is_cue_path, logical_path};
use crate::model::{CueSegment, ScanPhase};
use anyhow::{Context, Result, anyhow, bail};
use rivu_audio::{MediaInfo, SourceIdentity, probe_file};
use rivu_format::input::open_regular_file;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Cooperative cancellation shared by the scan owner and its async preparation.
#[derive(Clone, Debug, Default)]
pub struct ScanCancellation {
    token: CancellationToken,
}

impl ScanCancellation {
    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub(crate) async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("scan cancelled");
        }
        Ok(())
    }
}

// Reserved in modified_ns for unavailable/out-of-range mtimes, including the
// sentinel instant itself. This key must never authorize cache reuse; epoch 0 is
// a real timestamp. All other representable signed nanoseconds remain exact.
pub(super) const UNCACHEABLE_MODIFIED_NS: i64 = i64::MIN;

pub(super) fn cache_modified_ns(modified: Option<SystemTime>) -> i64 {
    let Some(modified) = modified else {
        return UNCACHEABLE_MODIFIED_NS;
    };
    let nanoseconds = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).ok(),
        Err(error) => i128::try_from(error.duration().as_nanos())
            .ok()
            .map(|nanoseconds| -nanoseconds),
    };
    nanoseconds
        .and_then(|nanoseconds| i64::try_from(nanoseconds).ok())
        .unwrap_or(UNCACHEABLE_MODIFIED_NS)
}

/// The filesystem information and cached probe result known to the database.
#[derive(Clone, Debug)]
pub struct KnownFile {
    pub size: u64,
    /// Signed Unix nanoseconds; i64::MIN is reserved for uncacheable mtimes.
    pub modified_ns: i64,
    pub fingerprint: Option<String>,
    pub media: Option<MediaInfo>,
    pub cue: Option<CueSegment>,
}

impl KnownFile {
    pub(super) fn matches_metadata(&self, size: u64, modified_ns: i64) -> bool {
        modified_ns != UNCACHEABLE_MODIFIED_NS
            && self.size == size
            && self.modified_ns == modified_ns
    }
}

#[derive(Clone, Debug)]
pub struct ScanRecord {
    pub path: PathBuf,
    pub size: u64,
    /// Signed Unix nanoseconds; i64::MIN is reserved for uncacheable mtimes.
    pub modified_ns: i64,
    pub fingerprint: Option<String>,
    pub media: MediaInfo,
    pub cue: Option<CueSegment>,
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
        let inspected = tokio::task::spawn_blocking(move || {
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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(scan_paths_with_cues(
            paths,
            |paths| std::future::ready(known_sources(&paths)),
            ffmpeg_enabled,
            library_roots,
            max_depth,
            (cancellation, None),
            progress,
        ))
}

pub(crate) async fn scan_paths_with_cues<Fut>(
    paths: &[PathBuf],
    mut known_sources: impl FnMut(Vec<PathBuf>) -> Fut,
    ffmpeg_enabled: bool,
    library_roots: &[PathBuf],
    max_depth: u32,
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
        let metadata = match tokio::task::spawn_blocking({
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
    let files = tokio::task::spawn_blocking(move || {
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
            let sheet = read_cue(path, library_roots, cancellation, sheets.as_mut()).await;
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
    let result = tokio::task::spawn_blocking(move || {
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
    let entries = tokio::task::spawn_blocking(move || fs::read_dir(root_path))
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
        let (next_stack, discoveries) = tokio::task::spawn_blocking(move || {
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
            let bytes =
                super::input::read_input_async(path, super::input::MAX_CUE_BYTES, cancellation)
                    .await
                    .with_context(|| format!("Reading CUE {}", path.display()))?;
            let path = path.to_path_buf();
            let cancellation = cancellation.clone();
            tokio::task::spawn_blocking(move || {
                super::cue::parse_input(&bytes, &path, &cancellation)
            })
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

pub(crate) const SOURCE_BATCH_SIZE: usize = 32;
const DIRECTORY_BATCH_SIZE: usize = 64;
const HASH_CHUNK_BYTES: usize = 128 * 1024;
// One blocking turn performs at most 32 probes/read steps and hashes at most 4 MiB.
const SOURCE_STEPS_PER_TURN: usize = 32;

struct PendingSource {
    path: PathBuf,
    file: fs::File,
    identity: SourceIdentity,
    size: u64,
    modified_ns: i64,
    known: Option<KnownFile>,
    initialized: bool,
    media: Option<MediaInfo>,
    fingerprint: Option<String>,
    hasher: Option<blake3::Hasher>,
    hashed: u64,
}

impl PendingSource {
    fn step(
        &mut self,
        ffmpeg_enabled: bool,
        cancellation: &ScanCancellation,
        scratch: &mut [u8],
    ) -> Result<Option<ScanRecord>> {
        cancellation.check()?;
        let size = self.size;
        let modified_ns = self.modified_ns;
        if !self.initialized {
            let unchanged = self
                .known
                .take()
                .filter(|known| known.matches_metadata(size, modified_ns));
            if let Some(mut known) = unchanged {
                self.fingerprint = known.fingerprint.take();
                self.media = known.media.take().filter(|media| {
                    known.cue.is_none()
                        && (!ffmpeg_enabled || (media.channels != 0 && media.sample_rate != 0))
                });
            }
            if self.media.is_none() {
                let file = self.file.try_clone()?;
                let media = probe_file(&self.path, file, ffmpeg_enabled).context("probe audio");
                self.media = Some(media?);
                cancellation.check()?;
            }
            if self.fingerprint.is_none() {
                // A duplicated probe fd shares its seek offset: rewind the same object.
                self.file
                    .rewind()
                    .with_context(|| format!("rewind {} for hashing", self.path.display()))?;
                self.hasher = Some(blake3::Hasher::new());
            }
            self.initialized = true;
        }
        if self.fingerprint.is_none() && self.hashed < size {
            let length = (size - self.hashed).min(scratch.len() as u64) as usize;
            let hasher = self
                .hasher
                .as_mut()
                .context("Audio hash was not initialized")?;
            let chunk = hash_chunk(&mut self.file, &mut scratch[..length], hasher, cancellation);
            match chunk.with_context(|| format!("hash {}", self.path.display()))? {
                None => return Ok(None),
                Some(0) => self.hashed = size,
                Some(count) => {
                    self.hashed += count as u64;
                    if self.hashed < size {
                        return Ok(None);
                    }
                }
            }
        }
        cancellation.check()?;
        if SourceIdentity::from_metadata(&self.file.metadata()?) != self.identity {
            bail!(
                "Audio source {} changed while scanning",
                self.path.display()
            );
        }
        cancellation.check()?;
        let fingerprint = match self.fingerprint.take() {
            Some(fingerprint) => fingerprint,
            None => self
                .hasher
                .as_ref()
                .context("Audio hash was not initialized")?
                .finalize()
                .to_hex()
                .to_string(),
        };
        Ok(Some(ScanRecord {
            path: std::mem::take(&mut self.path),
            size,
            modified_ns,
            fingerprint: Some(fingerprint),
            media: self.media.take().context("Audio probe returned no media")?,
            cue: None,
        }))
    }
}

pub(super) async fn prepare_sources<Fut>(
    paths: Vec<PathBuf>,
    known_sources: &mut impl FnMut(Vec<PathBuf>) -> Fut,
    sources: &mut HashMap<PathBuf, std::result::Result<ScanRecord, String>>,
    ffmpeg_enabled: bool,
    cancellation: &ScanCancellation,
    scratch: &mut Vec<u8>,
) -> Result<()>
where
    Fut: Future<Output = Result<Vec<Option<KnownFile>>>>,
{
    let mut paths = paths.into_iter();
    loop {
        cancellation.check()?;
        let batch: Vec<_> = paths.by_ref().take(SOURCE_BATCH_SIZE).collect();
        if batch.is_empty() {
            return Ok(());
        }
        let token = cancellation.clone();
        let admitted = tokio::task::spawn_blocking(move || {
            let mut admitted = Vec::with_capacity(batch.len());
            for path in batch {
                token.check()?;
                let opened = (|| {
                    path.to_str().context("Audio source path is not UTF-8")?;
                    let file = open_regular_file(&path)?;
                    let metadata = file.metadata()?;
                    Ok::<_, anyhow::Error>((file, metadata))
                })();
                admitted.push((path, opened));
            }
            token.check()?;
            Ok::<_, anyhow::Error>(admitted)
        })
        .await
        .context("Scan source admission worker failed")??;
        cancellation.check()?;
        let mut opened = Vec::with_capacity(admitted.len());
        let mut lookup_paths = Vec::with_capacity(admitted.len());
        for (path, admission) in admitted {
            match admission {
                Ok((file, metadata)) => {
                    lookup_paths.push(path.clone());
                    opened.push((path, file, metadata));
                }
                Err(error) => {
                    sources.insert(path, Err(format!("{error:#}")));
                }
            }
        }
        if opened.is_empty() {
            continue;
        }
        // No blocking slot is held across this Core request. Keep the admitted fds.
        let known = known_sources(lookup_paths).await;
        cancellation.check()?;
        let known = match known {
            Ok(known) => known,
            Err(error) => {
                let error = format!("{error:#}");
                for (path, _, _) in opened {
                    sources.insert(path, Err(error.clone()));
                }
                continue;
            }
        };
        anyhow::ensure!(
            known.len() == opened.len(),
            "Scan source lookup batch length mismatch"
        );
        let mut pending: std::collections::VecDeque<_> = opened
            .into_iter()
            .zip(known)
            .map(|((path, file, metadata), known)| PendingSource {
                path,
                file,
                identity: SourceIdentity::from_metadata(&metadata),
                size: metadata.len(),
                modified_ns: cache_modified_ns(metadata.modified().ok()),
                known,
                initialized: false,
                media: None,
                fingerprint: None,
                hasher: None,
                hashed: 0,
            })
            .collect();
        while !pending.is_empty() {
            cancellation.check()?;
            let token = cancellation.clone();
            let mut buffer = std::mem::take(scratch);
            let (remaining, completed, buffer) = tokio::task::spawn_blocking(move || {
                let mut completed = Vec::new();
                for _ in 0..SOURCE_STEPS_PER_TURN {
                    token.check()?;
                    let Some(source) = pending.front_mut() else {
                        break;
                    };
                    match source.step(ffmpeg_enabled, &token, &mut buffer) {
                        Ok(Some(record)) => {
                            pending.pop_front();
                            completed.push((record.path.clone(), Ok(record)));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            token.check()?;
                            let source = pending.pop_front().expect("front source is present");
                            completed.push((source.path, Err(format!("{error:#}"))));
                        }
                    }
                }
                token.check()?;
                Ok::<_, anyhow::Error>((pending, completed, buffer))
            })
            .await
            .context("Scan source batch worker failed")??;
            *scratch = buffer;
            pending = remaining;
            cancellation.check()?;
            sources.extend(completed);
        }
    }
}

pub(super) fn hash_chunk(
    reader: &mut impl Read,
    scratch: &mut [u8],
    hasher: &mut blake3::Hasher,
    cancellation: &ScanCancellation,
) -> Result<Option<usize>> {
    cancellation.check()?;
    let read = reader.read(scratch);
    cancellation.check()?;
    let count = match read {
        Ok(count) => count,
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    hasher.update(&scratch[..count]);
    Ok(Some(count))
}

#[cfg(test)]
mod recheck_tests {
    use super::*;

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

    #[tokio::test]
    async fn exact_recheck_never_expands_inputs_and_only_confirms_absence() {
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
    }

    #[tokio::test]
    async fn exact_recheck_reuses_cache_and_force_callback_bypasses_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cached.wav");
        fs::write(&path, b"intentionally unprobeable cached bytes").unwrap();
        let known = cached(&path);
        let result = recheck_sources(
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
    }

    #[tokio::test]
    async fn exact_recheck_preserves_probe_failures_and_observes_cancellation_after_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe.wav");
        fs::write(&path, b"unreadable probe").unwrap();
        let failed = recheck_sources(
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
    }
}
