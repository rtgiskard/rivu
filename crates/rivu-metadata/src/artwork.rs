//! Local, best-effort artwork shared by the application session. Cache-only
//! reads never probe media; blocking loads belong on existing background workers.
//! No decoder or audio output is opened; failures never propagate into playback.

pub mod cache_paths;

use parking_lot::{Mutex, RwLock};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};

/// Borrowed content identity and current media location for an artwork request.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub id: i64,
    pub fingerprint: Option<&'a str>,
    pub path: &'a Path,
}

const MAX_ARTWORK_ENTRIES: usize = 64;
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

struct ArtworkFile {
    uri: String,
    path: Arc<Path>,
    max_image_bytes: usize,
}

/// Holds a published file and protects its URI until it is released.
#[derive(Clone)]
pub struct Artwork(Arc<ArtworkFile>);

impl Artwork {
    pub fn uri(&self) -> &str {
        &self.0.uri
    }

    pub fn path(&self) -> Arc<Path> {
        Arc::clone(&self.0.path)
    }

    /// Immutable encoded admission policy of the session that published this file.
    pub fn max_image_bytes(&self) -> usize {
        self.0.max_image_bytes
    }
}

impl std::fmt::Debug for Artwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Artwork").field(&self.uri()).finish()
    }
}

impl PartialEq for Artwork {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Artwork {}

/// Snapshot of a manual cache inventory or age-based cleanup.
#[derive(Debug, Default)]
pub struct ArtworkCacheReport {
    pub files: u64,
    pub bytes: u64,
    pub expired_files: u64,
    pub expired_bytes: u64,
    pub protected_files: u64,
    pub protected_bytes: u64,
    pub removed_files: u64,
    pub removed_bytes: u64,
    pub failed_files: u64,
    pub first_error: Option<String>,
}

impl ArtworkCacheReport {
    fn failure(&mut self, path: &Path, error: io::Error) {
        self.failed_files += 1;
        if self.first_error.is_none() {
            self.first_error = Some(format!("{}: {error}", path.display()));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkIdentity {
    id: i64,
    fingerprint: Option<String>,
    path: PathBuf,
}

impl ArtworkIdentity {
    pub fn new(source: Source<'_>) -> Self {
        Self {
            id: source.id,
            fingerprint: source.fingerprint.map(str::to_owned),
            path: source.path.to_owned(),
        }
    }

    pub fn matches(&self, source: Source<'_>) -> bool {
        self.matches_content(source) && self.path == source.path
    }

    fn matches_content(&self, source: Source<'_>) -> bool {
        self.id == source.id && self.fingerprint.as_deref() == source.fingerprint
    }
}

struct CachedArtwork {
    identity: ArtworkIdentity,
    lookup: OnceLock<ArtworkLookup>,
}

enum ArtworkSource {
    Embedded(Arc<ArtworkFile>),
    Directory(Option<Arc<ArtworkFile>>),
}

struct ArtworkLookup {
    image: Option<Arc<ArtworkFile>>,
    source_path: Option<PathBuf>,
}

impl ArtworkLookup {
    fn applies_to(&self, path: &Path) -> bool {
        self.source_path
            .as_deref()
            .is_none_or(|source_path| source_path == path)
    }
}

impl CachedArtwork {
    fn matches(&self, source: Source<'_>) -> bool {
        self.identity.matches_content(source)
            && self
                .lookup
                .get()
                .is_none_or(|lookup| lookup.applies_to(source.path))
    }
}

#[derive(Default)]
struct ArtworkCache {
    items: HashMap<i64, Arc<CachedArtwork>>,
    insertion_order: VecDeque<Arc<CachedArtwork>>,
}

impl ArtworkCache {
    fn find(&self, source: Source<'_>) -> Option<&Arc<CachedArtwork>> {
        self.items
            .get(&source.id)
            .filter(|entry| entry.matches(source))
            .or_else(|| {
                self.insertion_order
                    .iter()
                    .find(|entry| entry.matches(source))
            })
    }

    fn trim(&mut self) {
        while self.insertion_order.len() > MAX_ARTWORK_ENTRIES {
            let Some(position) = self.insertion_order.iter().position(|entry| {
                let indexed = self
                    .items
                    .get(&entry.identity.id)
                    .is_some_and(|latest| Arc::ptr_eq(latest, entry));
                // In-flight loads hold the lookup; published Artwork handles
                // hold its file directly. Either keeps this version resident.
                Arc::strong_count(entry) == 1 + usize::from(indexed)
                    && entry
                        .lookup
                        .get()
                        .and_then(|lookup| lookup.image.as_ref())
                        .is_none_or(|file| Arc::strong_count(file) == 1)
            }) else {
                break;
            };
            let entry = self
                .insertion_order
                .remove(position)
                .expect("FIFO position");
            if self
                .items
                .get(&entry.identity.id)
                .is_some_and(|latest| Arc::ptr_eq(latest, &entry))
            {
                self.items.remove(&entry.identity.id);
            }
        }
    }

    fn protects(&self, path: &Path) -> bool {
        self.insertion_order.iter().any(|entry| {
            entry
                .lookup
                .get()
                .and_then(|lookup| lookup.image.as_ref())
                .is_some_and(|file| file.path.as_ref() == path)
        })
    }
}

pub struct ArtworkManager {
    directory: PathBuf,
    max_image_bytes: usize,
    default_image: Option<&'static [u8]>,
    default: OnceLock<Option<Artwork>>,
    // Background acquisition holds a reader through file creation and lookup
    // publication. Cleanup holds a writer only for one file at a time. Cache-
    // only reads never take this gate, and no lookup lock covers filesystem I/O.
    maintenance: RwLock<()>,
    entries: Mutex<ArtworkCache>,
}

impl ArtworkManager {
    pub fn new(data_dir: &Path) -> Self {
        let directory = data_dir.join("artwork");
        Self {
            directory,
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            default_image: None,
            default: OnceLock::new(),
            maintenance: RwLock::new(()),
            entries: Mutex::new(ArtworkCache::default()),
        }
    }

    /// Provide the PNG exported on explicit fallback requests. Without one,
    /// default-artwork reads and exports return `None` without filesystem work.
    pub fn with_default_image(mut self, bytes: &'static [u8]) -> Self {
        self.default_image = Some(bytes);
        self
    }

    /// Configure the encoded artwork limit before sharing this session manager.
    pub fn with_max_image_bytes(mut self, bytes: usize) -> Self {
        self.max_image_bytes = bytes;
        self
    }

    /// Default-artwork cache read; never creates a file or waits for an export.
    pub fn cached_default(&self) -> Option<Artwork> {
        self.default.get().and_then(Clone::clone)
    }

    /// Explicit blocking default-artwork export. Failure is logged and remembered once per session.
    pub fn export_default(&self) -> Option<Artwork> {
        let bytes = self.default_image?;
        let _acquisition = self.maintenance.read();
        self.default
            .get_or_init(|| {
                let path = cache_paths::original_path(
                    &self.directory.join("cache"),
                    blake3::hash(bytes),
                    "png",
                );
                match write_image(&path, bytes) {
                    Ok(uri) => Some(Artwork(artwork_file(uri, self.max_image_bytes))),
                    Err(error) => {
                        tracing::warn!(%error, "mpris_default_artwork_export_failed");
                        None
                    }
                }
            })
            .clone()
    }

    /// Cache-only acquisition: no filesystem work or probe waiting.
    pub fn cached(&self, source: Source<'_>) -> Option<Artwork> {
        self.cached_result(source).flatten()
    }

    /// Cache-only acquisition: `None` is cold/in-flight, `Some(None)` a completed
    /// miss, and `Some(Some(_))` a positive lease acquired before eviction can
    /// remove it. Never performs filesystem work or waits for a probe.
    pub fn cached_result(&self, source: Source<'_>) -> Option<Option<Artwork>> {
        let cache = self.entries.lock();
        let lookup = cache.find(source)?.lookup.get()?;
        lookup
            .applies_to(source.path)
            .then(|| lookup.image.as_ref().map(|file| Artwork(Arc::clone(file))))
    }

    /// Blocking singleflight load; a late old version never replaces its successor.
    pub fn load(&self, source: Source<'_>) -> Option<Artwork> {
        self.load_with(source, || {
            self.source_uri(source.path, self.max_image_bytes)
        })
    }

    fn load_with(
        &self,
        source: Source<'_>,
        probe: impl FnOnce() -> ArtworkSource,
    ) -> Option<Artwork> {
        let _acquisition = self.maintenance.read();
        let mut probe = Some(probe);
        loop {
            let cached = {
                let mut cache = self.entries.lock();
                if let Some(entry) = cache.find(source) {
                    Arc::clone(entry)
                } else {
                    let entry = Arc::new(CachedArtwork {
                        identity: ArtworkIdentity::new(source),
                        lookup: OnceLock::new(),
                    });
                    cache.items.insert(source.id, Arc::clone(&entry));
                    cache.insertion_order.push_back(Arc::clone(&entry));
                    cache.trim();
                    entry
                }
            };
            let lookup = cached.lookup.get_or_init(|| {
                let (image, source_path) = match probe.take().expect("one source probe")() {
                    ArtworkSource::Embedded(image) => (Some(image), None),
                    ArtworkSource::Directory(image) => (image, Some(source.path.to_owned())),
                };
                ArtworkLookup { source_path, image }
            });
            // A concurrent request may have shared an unfinished old-path probe.
            // Only embedded content is reusable without checking the new directory.
            if !lookup.applies_to(source.path) {
                continue;
            }
            let result = lookup.image.as_ref().map(|file| Artwork(Arc::clone(file)));
            self.entries.lock().trim();
            return result;
        }
    }

    /// Explicit disk inventory/cleanup. Startup and ordinary loads never enumerate
    /// the cache, and disk space has no automatic budget or session-wide index.
    pub fn maintain(&self, older_than: Duration, clean: bool) -> io::Result<ArtworkCacheReport> {
        // Published URIs use canonical paths. Resolve only for explicit
        // maintenance so relative/symlinked data directories compare correctly.
        let directory = match fs::canonicalize(self.directory.join("cache")) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ArtworkCacheReport::default());
            }
            Err(error) => return Err(error),
        };
        let entries = fs::read_dir(&directory)?;
        let now = SystemTime::now();
        let mut report = ArtworkCacheReport::default();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.failure(&directory, error);
                    continue;
                }
            };
            let name = entry.file_name();
            let path = entry.path();
            if name.to_str().is_some_and(|name| lowercase_hex(name, 2)) {
                // Exactly one real bucket level; never follow directory symlinks
                // or descend into arbitrary directories or nested buckets.
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => {}
                    Ok(_) => continue,
                    Err(error) => {
                        report.failure(&path, error);
                        continue;
                    }
                }
                let children = match fs::read_dir(&path) {
                    Ok(children) => children,
                    Err(error) => {
                        report.failure(&path, error);
                        continue;
                    }
                };
                for child in children {
                    match child {
                        Ok(child) if managed_filename(&child.file_name()) => {
                            self.maintain_file(&child.path(), now, older_than, clean, &mut report);
                        }
                        Ok(_) => {}
                        Err(error) => report.failure(&path, error),
                    }
                }
            }
        }
        Ok(report)
    }

    fn maintain_file(
        &self,
        path: &Path,
        now: SystemTime,
        older_than: Duration,
        clean: bool,
        report: &mut ArtworkCacheReport,
    ) {
        // Serialize original-file creation/registration with removal. A decoded
        // thumbnail holds pixels, not a URI lease: its disposable disk file can
        // expire independently of a resident original, and a worker racing
        // cleanup regenerates it on a cache-open miss.
        let _removal = clean.then(|| self.maintenance.write());
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return,
            Err(error) => {
                report.failure(path, error);
                return;
            }
        };
        report.files += 1;
        report.bytes += metadata.len();
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) => {
                report.failure(path, error);
                return;
            }
        };
        if !now
            .duration_since(modified)
            .is_ok_and(|age| age >= older_than)
        {
            return;
        }
        report.expired_files += 1;
        report.expired_bytes += metadata.len();
        if self.entries.lock().protects(path)
            || self
                .default
                .get()
                .and_then(Option::as_ref)
                .is_some_and(|artwork| artwork.0.path.as_ref() == path)
        {
            report.protected_files += 1;
            report.protected_bytes += metadata.len();
            return;
        }
        if clean {
            match fs::remove_file(path) {
                Ok(()) => {
                    report.removed_files += 1;
                    report.removed_bytes += metadata.len();
                }
                Err(error) => report.failure(path, error),
            }
        }
    }

    fn source_uri(&self, path: &Path, limit: usize) -> ArtworkSource {
        if let Ok(Some(image)) = self.embedded_uri(path, limit) {
            ArtworkSource::Embedded(image)
        } else {
            ArtworkSource::Directory(self.directory_uri(path, limit))
        }
    }

    fn directory_uri(&self, path: &Path, limit: usize) -> Option<Arc<ArtworkFile>> {
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut candidates: Vec<_> = fs::read_dir(directory)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let priority = directory_image_priority(&path)?;
                let extension = path.extension()?.to_str()?;
                if ![
                    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico",
                ]
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
                {
                    return None;
                }
                let kind = entry.file_type().ok()?;
                (kind.is_file() || kind.is_symlink()).then_some((priority, path))
            })
            .collect();
        candidates.sort_unstable_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| {
                let left = left.1.file_name().expect("directory image filename");
                let right = right.1.file_name().expect("directory image filename");
                left.as_encoded_bytes()
                    .iter()
                    .map(u8::to_ascii_lowercase)
                    .cmp(right.as_encoded_bytes().iter().map(u8::to_ascii_lowercase))
                    .then_with(|| left.cmp(right))
            })
        });
        let mut bytes = Vec::new();
        for (_, candidate) in candidates {
            let Ok(file) = rivu_utils::regular_file::open_regular_file(&candidate) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if metadata.len() > limit as u64 {
                continue;
            }
            bytes.clear();
            // A file may grow after metadata(): the reader still admits at most
            // the image budget plus one byte to detect overflow.
            if file.take(limit as u64 + 1).read_to_end(&mut bytes).is_ok()
                && let Some(image) = self.image_uri(&bytes, limit)
            {
                return Some(image);
            }
        }
        None
    }

    fn embedded_uri(&self, path: &Path, limit: usize) -> Result<Option<Arc<ArtworkFile>>, ()> {
        let file = rivu_utils::regular_file::open_regular_file(path).map_err(|_| ())?;
        crate::read_artwork(file, limit, |bytes| self.image_uri(bytes, limit)).map_err(|error| {
            tracing::debug!(path = %path.display(), error = %error, "Reading embedded artwork");
        })
    }

    fn image_uri(&self, bytes: &[u8], limit: usize) -> Option<Arc<ArtworkFile>> {
        if bytes.len() > limit {
            return None;
        }
        // Sniff the bytes rather than trusting tag MIME types or file names.
        // Keep the original image; desktop clients handle decoding and scaling.
        let extension = match image::guess_format(bytes).ok()? {
            image::ImageFormat::Png => "png",
            image::ImageFormat::Jpeg => "jpg",
            image::ImageFormat::Gif => "gif",
            image::ImageFormat::WebP => "webp",
            image::ImageFormat::Bmp => "bmp",
            image::ImageFormat::Tiff => "tiff",
            image::ImageFormat::Ico => "ico",
            _ => return None,
        };
        let hash = blake3::hash(bytes);
        let path = cache_paths::original_path(&self.directory.join("cache"), hash, extension);
        let uri = write_image(&path, bytes).ok()?;
        Some(artwork_file(uri, self.max_image_bytes))
    }
}

fn directory_image_priority(path: &Path) -> Option<u8> {
    match path.file_stem().and_then(|stem| stem.to_str()) {
        Some(stem) if stem.eq_ignore_ascii_case("cover") => Some(0),
        Some(stem) if stem.eq_ignore_ascii_case("folder") => Some(1),
        Some(stem) if stem.eq_ignore_ascii_case("front") => Some(2),
        _ => None,
    }
}

fn artwork_file(uri: String, max_image_bytes: usize) -> Arc<ArtworkFile> {
    let path = url::Url::parse(&uri)
        .expect("artwork file URI")
        .to_file_path()
        .expect("local artwork");
    Arc::new(ArtworkFile {
        uri,
        path: Arc::from(path),
        max_image_bytes,
    })
}

fn managed_filename(name: &std::ffi::OsStr) -> bool {
    let Some((stem, extension)) = name.to_str().and_then(|name| name.rsplit_once('.')) else {
        return false;
    };
    let hash = if let Some((hash, tier)) = stem.split_once('-') {
        if extension != "webp" || !matches!(tier, "256" | "512" | "1024") {
            return false;
        }
        hash
    } else {
        stem
    };
    lowercase_hex(hash, 30)
        && matches!(
            extension,
            "png" | "jpg" | "gif" | "webp" | "bmp" | "tiff" | "ico"
        )
}

fn lowercase_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn file_uri(path: &Path) -> io::Result<String> {
    let absolute = fs::canonicalize(path)?;
    url::Url::from_file_path(absolute)
        .map(String::from)
        .map_err(|_| io::Error::other("Artwork path cannot be represented as a file URI"))
}

fn write_image(path: &Path, data: &[u8]) -> io::Result<String> {
    match fs::symlink_metadata(path) {
        Ok(_) => return reuse_image(path, data),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Artwork has no directory"))?;
    fs::create_dir_all(parent)?;
    // Publish only a complete file and never clobber another publisher. The
    // cache is disposable: no fsync. A race winner still needs a content check.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(data)?;
    match temporary.persist_noclobber(path) {
        Ok(_) => file_uri(path),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => reuse_image(path, data),
        Err(error) => Err(error.error),
    }
}

fn reuse_image(path: &Path, data: &[u8]) -> io::Result<String> {
    let mismatch = || io::Error::new(io::ErrorKind::InvalidData, "Artwork cache content mismatch");
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() != data.len() as u64 {
        return Err(mismatch());
    }
    let file = rivu_utils::regular_file::open_regular_file(path)?;
    // Stream at most the expected length plus one byte, including if the file
    // grows after metadata. Comparison uses fixed stack space, not a second image.
    let mut reader = file.take(data.len() as u64 + 1);
    let mut buffer = [0; 8192];
    let mut offset = 0;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if data.get(offset..offset + count) != Some(&buffer[..count]) {
            return Err(mismatch());
        }
        offset += count;
    }
    if offset != data.len() {
        return Err(mismatch());
    }
    file_uri(path)
}

#[cfg(any(test, feature = "test-support"))]
mod fixtures;

/// Deterministic media fixtures for application artwork tests.
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use super::fixtures::{block_probe, budget_unsynchronised_wav, pictures_wav, tagged_wav};
}

#[cfg(test)]
mod tests;
