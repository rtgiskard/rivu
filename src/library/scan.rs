use super::paths::{is_audio_path, is_cue_path, logical_path};
use crate::audio::probe;
use crate::model::{CueSegment, MediaInfo, ScanPhase};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Cooperative cancellation shared by the scan owner and its filesystem worker.
#[derive(Clone, Debug)]
pub struct ScanCancellation {
    state: Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
    sender: parking_lot::Mutex<Option<crossbeam_channel::Sender<()>>>,
    receiver: crossbeam_channel::Receiver<()>,
}

impl Default for ScanCancellation {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(0);
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                sender: parking_lot::Mutex::new(Some(sender)),
                receiver,
            }),
        }
    }
}

impl ScanCancellation {
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Relaxed);
        // Disconnecting wakes every wait without consuming a one-shot signal.
        self.state.sender.lock().take();
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Relaxed)
    }

    pub(crate) fn cancelled(&self) -> &crossbeam_channel::Receiver<()> {
        &self.state.receiver
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
    pub track_id: i64,
    pub path: PathBuf,
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
    pub roots: Vec<PathBuf>,
    pub records: Vec<ScanRecord>,
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
/// `known_source` looks up only a source actually encountered by this scan.
/// `library_roots` are normalized absolute logical paths, as published by the core.
/// `max_depth` is relative to each requested directory, which is itself depth 0.
/// Cancellation returns an error, never a partial result. Checks surround blocking
/// probes and lookups, but an in-flight probe itself is not interruptible.
pub fn scan_paths(
    paths: &[PathBuf],
    known_source: impl FnMut(&Path) -> Result<Option<KnownFile>>,
    ffmpeg_enabled: bool,
    library_roots: &[PathBuf],
    max_depth: u32,
    cancellation: &ScanCancellation,
    progress: impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) -> Result<ScanResult> {
    scan_paths_with_cues(
        paths,
        known_source,
        ffmpeg_enabled,
        library_roots,
        max_depth,
        (cancellation, None),
        progress,
    )
}

pub(crate) fn scan_paths_with_cues(
    paths: &[PathBuf],
    mut known_source: impl FnMut(&Path) -> Result<Option<KnownFile>>,
    ffmpeg_enabled: bool,
    library_roots: &[PathBuf],
    max_depth: u32,
    (cancellation, mut sheets): (
        &ScanCancellation,
        Option<HashMap<PathBuf, crate::cue::CueSheet>>,
    ),
    mut progress: impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) -> Result<ScanResult> {
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
        let metadata = match fs::metadata(&root) {
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
        if metadata.is_file() || (!metadata.is_dir() && is_cue_path(&root)) {
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
        } else if metadata.is_dir() {
            let max_walk_depth = (max_depth as usize).saturating_add(1);
            let mut entries = WalkDir::new(&root)
                .follow_links(true)
                .max_depth(max_walk_depth)
                .into_iter();
            loop {
                cancellation.check()?;
                let Some(entry) = entries.next() else {
                    break;
                };
                cancellation.check()?;
                match entry {
                    Ok(entry)
                        if entry.depth() > max_depth as usize && entry.file_type().is_dir() =>
                    {
                        entries.skip_current_dir();
                        let report = result.push_skipped_directory(root_index, entry.path());
                        if report {
                            tracing::warn!(
                                path = %entry.path().display(),
                                max_depth,
                                "scan_depth_limit_reached"
                            );
                        }
                        result.push_error(|| {
                            format!(
                                "{}: directory exceeds scan depth limit {max_depth}",
                                entry.path().display()
                            )
                        });
                        progress(
                            ScanPhase::Discovering,
                            files.len(),
                            None,
                            result.errors.len(),
                            Some(entry.path()),
                        );
                    }
                    Ok(entry)
                        if (entry.file_type().is_file() && is_audio_path(entry.path()))
                            || (!entry.file_type().is_dir() && is_cue_path(entry.path())) =>
                    {
                        if files.insert(entry.path().to_path_buf()) {
                            progress(
                                ScanPhase::Discovering,
                                files.len(),
                                None,
                                result.errors.len(),
                                Some(entry.path()),
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        result.push_uncertainty(ScanUncertainty::Subtree {
                            root_index,
                            path: error.path().unwrap_or(&root).to_path_buf(),
                        });
                        result.push_error(|| format!("{}: {error}", root.display()));
                        progress(
                            ScanPhase::Discovering,
                            files.len(),
                            None,
                            result.errors.len(),
                            Some(root.as_path()),
                        );
                    }
                }
            }
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
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort();
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
    for sheet in files.iter().filter(|path| is_cue_path(path)) {
        cancellation.check()?;
        match scan_cue(
            sheet,
            &mut known_source,
            &mut sources,
            ffmpeg_enabled,
            library_roots,
            cancellation,
            sheets.as_mut(),
        ) {
            Ok(records) => {
                suppressed.extend(records.iter().map(|record| record.path.clone()));
                result.records.extend(records);
            }
            Err(error) => {
                cancellation.check()?;
                result.push_uncertainty(ScanUncertainty::CueSheet(sheet.clone()));
                result.push_error(|| format!("{}: {error:#}", sheet.display()));
            }
        }
        processed += 1;
        progress(
            ScanPhase::ReadingMetadata,
            processed,
            Some(total),
            result.errors.len(),
            Some(sheet.as_path()),
        );
    }
    for (path, source) in &sources {
        if source.is_err() {
            result.push_uncertainty(ScanUncertainty::File(path.clone()));
        }
    }
    for path in files.iter().filter(|path| is_audio_path(path)) {
        cancellation.check()?;
        if suppressed.contains(path) && !explicit_audio.contains(path) {
            processed += 1;
            progress(
                ScanPhase::ReadingMetadata,
                processed,
                Some(total),
                result.errors.len(),
                Some(path.as_path()),
            );
            continue;
        }
        // CUE sheets are finished, so ordinary files can consume their source snapshot.
        let record = match sources.remove(path) {
            Some(record) => record.map_err(|error| anyhow!("{error}")),
            None => lookup_and_scan_file(path, &mut known_source, ffmpeg_enabled, cancellation),
        };
        cancellation.check()?;
        match record {
            Ok(record) => result.records.push(record),
            Err(error) => {
                result.push_uncertainty(ScanUncertainty::File(path.clone()));
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
    cancellation.check()?;
    suppressed.retain(|path| !explicit_audio.contains(path));
    result.suppressed_sources = suppressed.into_iter().collect();
    result.suppressed_sources.sort();
    result.skipped_directories.sort_unstable_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.root_index.cmp(&b.root_index))
    });
    result.records.sort_by(|a, b| {
        let identity = |record: &ScanRecord| record.cue.as_ref().map(|cue| cue.number).unwrap_or(0);
        let a_path = a.cue.as_ref().map_or(&a.path, |cue| &cue.sheet);
        let b_path = b.cue.as_ref().map_or(&b.path, |cue| &cue.sheet);
        a_path
            .cmp(b_path)
            .then_with(|| identity(a).cmp(&identity(b)))
    });
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

fn source_record<'a>(
    path: PathBuf,
    known_source: &mut impl FnMut(&Path) -> Result<Option<KnownFile>>,
    sources: &'a mut HashMap<PathBuf, std::result::Result<ScanRecord, String>>,
    ffmpeg_enabled: bool,
    cancellation: &ScanCancellation,
) -> Result<&'a ScanRecord> {
    cancellation.check()?;
    sources
        .entry(path)
        .or_insert_with_key(|path| {
            lookup_and_scan_file(path, known_source, ffmpeg_enabled, cancellation)
                .map_err(|error| format!("{error:#}"))
        })
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn scan_cue(
    path: &Path,
    known_source: &mut impl FnMut(&Path) -> Result<Option<KnownFile>>,
    sources: &mut HashMap<PathBuf, std::result::Result<ScanRecord, String>>,
    ffmpeg_enabled: bool,
    library_roots: &[PathBuf],
    cancellation: &ScanCancellation,
    sheets: Option<&mut HashMap<PathBuf, crate::cue::CueSheet>>,
) -> Result<Vec<ScanRecord>> {
    cancellation.check()?;
    path.to_str().context("CUE sheet path is not UTF-8")?;
    let sheet = match sheets {
        Some(sheets) => sheets
            .remove(path)
            .context("Import CUE sheet was not prepared"),
        None => {
            let bytes = super::playlist_io::read_input(
                path,
                super::playlist_io::MAX_CUE_BYTES,
                cancellation,
            )
            .with_context(|| format!("Reading CUE {}", path.display()))?;
            crate::cue::parse_input(&bytes, path, cancellation)
        }
    };
    cancellation.check()?;
    let sheet = sheet?;
    let mut records = Vec::with_capacity(sheet.tracks.len());
    for track in sheet.tracks {
        cancellation.check()?;
        let source = logical_path(&track.file)
            .with_context(|| format!("resolve CUE source {}", track.file.display()))?;
        if !library_roots.iter().any(|root| source.starts_with(root)) {
            bail!(
                "CUE source {} is outside the configured library roots",
                source.display()
            );
        }
        let source = source_record(source, known_source, sources, ffmpeg_enabled, cancellation)?;
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

fn lookup_and_scan_file(
    path: &Path,
    known_source: &mut impl FnMut(&Path) -> Result<Option<KnownFile>>,
    ffmpeg_enabled: bool,
    cancellation: &ScanCancellation,
) -> Result<ScanRecord> {
    cancellation.check()?;
    let known = known_source(path);
    cancellation.check()?;
    scan_file(path, known?.as_ref(), ffmpeg_enabled, cancellation)
}

fn scan_file(
    path: &Path,
    known: Option<&KnownFile>,
    ffmpeg_enabled: bool,
    cancellation: &ScanCancellation,
) -> Result<ScanRecord> {
    cancellation.check()?;
    let metadata =
        fs::metadata(path).with_context(|| format!("read metadata for {}", path.display()))?;
    cancellation.check()?;
    let size = metadata.len();
    let modified_ns = cache_modified_ns(metadata.modified().ok());
    let unchanged = known.filter(|known| known.matches_metadata(size, modified_ns));
    let media = match unchanged
        .filter(|known| known.cue.is_none())
        .and_then(|known| known.media.as_ref())
        .filter(|media| !ffmpeg_enabled || (media.channels != 0 && media.sample_rate != 0))
    {
        Some(media) => media.clone(),
        None => {
            cancellation.check()?;
            let media = probe(path, ffmpeg_enabled).with_context(|| "probe audio");
            cancellation.check()?;
            media?
        }
    };
    let fingerprint = match unchanged.and_then(|known| known.fingerprint.as_ref()) {
        Some(fingerprint) => fingerprint.clone(),
        None => hash_file(path, cancellation)?,
    };
    cancellation.check()?;
    Ok(ScanRecord {
        path: path.to_path_buf(),
        size,
        modified_ns,
        fingerprint: Some(fingerprint),
        media,
        cue: None,
    })
}

fn hash_file(path: &Path, cancellation: &ScanCancellation) -> Result<String> {
    cancellation.check()?;
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    hash_reader(file, cancellation).with_context(|| format!("hash {}", path.display()))
}

pub(super) fn hash_reader(
    mut reader: impl Read,
    cancellation: &ScanCancellation,
) -> Result<String> {
    let mut buffer = [0; 128 * 1024];
    let mut hasher = blake3::Hasher::new();
    loop {
        cancellation.check()?;
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        cancellation.check()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
