//! Local, best-effort artwork shared by the application session. Cache-only
//! reads never probe media; blocking loads belong on existing background workers.
//! No decoder or audio output is opened; failures never propagate into playback.

pub(crate) mod cache;

use parking_lot::{Mutex, RwLock};
use rivu_protocol::model::Track;
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};
use symphonia::{
    core::{
        formats::{FormatOptions, TrackType, probe::Hint},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::{Metadata, MetadataOptions, StandardVisualKey},
    },
    default::get_probe,
};

pub(crate) static DEFAULT_IMAGE: &[u8] = include_bytes!("../data/icons/rivu.png");

const MAX_ARTWORK_ENTRIES: usize = 64;
pub(crate) const DEFAULT_MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

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

    pub(crate) fn path(&self) -> Arc<Path> {
        Arc::clone(&self.0.path)
    }

    /// Immutable encoded admission policy of the session that published this file.
    pub(crate) fn max_image_bytes(&self) -> usize {
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
pub(crate) struct ArtworkIdentity {
    id: i64,
    fingerprint: Option<String>,
    path: PathBuf,
}

impl ArtworkIdentity {
    pub(crate) fn for_track(track: &Track) -> Self {
        Self {
            id: track.id,
            fingerprint: track.fingerprint.clone(),
            path: track.path.clone(),
        }
    }

    pub(crate) fn matches(&self, track: &Track) -> bool {
        self.matches_content(track) && self.path == track.path
    }

    fn matches_content(&self, track: &Track) -> bool {
        self.id == track.id && self.fingerprint == track.fingerprint
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
    fn matches(&self, track: &Track) -> bool {
        self.identity.matches_content(track)
            && self
                .lookup
                .get()
                .is_none_or(|lookup| lookup.applies_to(&track.path))
    }
}

#[derive(Default)]
struct ArtworkCache {
    items: HashMap<i64, Arc<CachedArtwork>>,
    insertion_order: VecDeque<Arc<CachedArtwork>>,
}

impl ArtworkCache {
    fn find(&self, track: &Track) -> Option<&Arc<CachedArtwork>> {
        self.items
            .get(&track.id)
            .filter(|entry| entry.matches(track))
            .or_else(|| {
                self.insertion_order
                    .iter()
                    .find(|entry| entry.matches(track))
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
            default: OnceLock::new(),
            maintenance: RwLock::new(()),
            entries: Mutex::new(ArtworkCache::default()),
        }
    }

    /// Configure the encoded artwork limit before sharing this session manager.
    pub(crate) fn with_max_image_bytes(mut self, bytes: usize) -> Self {
        self.max_image_bytes = bytes;
        self
    }

    /// MPRIS-only cache read; never creates a file or waits for an export.
    pub(crate) fn cached_default(&self) -> Option<Artwork> {
        self.default.get().and_then(Clone::clone)
    }

    /// MPRIS-only blocking export. Failure is logged and remembered once per session.
    pub(crate) fn export_default(&self) -> Option<Artwork> {
        let _acquisition = self.maintenance.read();
        self.default
            .get_or_init(|| {
                let path = cache::original_path(
                    &self.directory.join("cache"),
                    blake3::hash(DEFAULT_IMAGE),
                    "png",
                );
                match write_image(&path, DEFAULT_IMAGE) {
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
    pub fn cached(&self, track: &Track) -> Option<Artwork> {
        let cache = self.entries.lock();
        cache
            .find(track)
            .filter(|entry| {
                entry
                    .lookup
                    .get()
                    .is_some_and(|lookup| lookup.applies_to(&track.path) && lookup.image.is_some())
            })
            .and_then(|entry| {
                entry
                    .lookup
                    .get()?
                    .image
                    .as_ref()
                    .map(|file| Artwork(Arc::clone(file)))
            })
    }

    pub fn is_loaded(&self, track: &Track) -> bool {
        self.entries.lock().find(track).is_some_and(|entry| {
            entry
                .lookup
                .get()
                .is_some_and(|lookup| lookup.applies_to(&track.path))
        })
    }

    /// Blocking singleflight load; a late old version never replaces its successor.
    pub fn load(&self, track: &Track) -> Option<Artwork> {
        self.load_with(track, || self.source_uri(&track.path, self.max_image_bytes))
    }

    fn load_with(&self, track: &Track, probe: impl FnOnce() -> ArtworkSource) -> Option<Artwork> {
        let _acquisition = self.maintenance.read();
        let mut probe = Some(probe);
        loop {
            let cached = {
                let mut cache = self.entries.lock();
                if let Some(entry) = cache.find(track) {
                    Arc::clone(entry)
                } else {
                    let entry = Arc::new(CachedArtwork {
                        identity: ArtworkIdentity::for_track(track),
                        lookup: OnceLock::new(),
                    });
                    cache.items.insert(track.id, Arc::clone(&entry));
                    cache.insertion_order.push_back(Arc::clone(&entry));
                    cache.trim();
                    entry
                }
            };
            let lookup = cached.lookup.get_or_init(|| {
                let (image, source_path) = match probe.take().expect("one source probe")() {
                    ArtworkSource::Embedded(image) => (Some(image), None),
                    ArtworkSource::Directory(image) => (image, Some(track.path.clone())),
                };
                ArtworkLookup { source_path, image }
            });
            // A concurrent request may have shared an unfinished old-path probe.
            // Only embedded content is reusable without checking the new directory.
            if !lookup.applies_to(&track.path) {
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
            if managed_filename(&name, false) {
                self.maintain_file(&path, now, older_than, clean, &mut report);
            } else if name.to_str().is_some_and(|name| lowercase_hex(name, 2)) {
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
                        Ok(child) if managed_filename(&child.file_name(), true) => {
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
            let Ok(file) = rivu_format::input::open_regular_file(&candidate) else {
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
        let file = rivu_format::input::open_regular_file(path).map_err(|_| ())?;
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let mut budget = symphonia_metadata::budget::MetadataBudget::for_artwork(limit);
        let mut format = budget
            .with(|| {
                get_probe().probe(
                    &hint,
                    MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default()),
                    FormatOptions::default(),
                    MetadataOptions::default(),
                )
            })
            .map_err(|_| ())?;
        let track_id = format
            .default_track(TrackType::Audio)
            .map(|track| u64::from(track.id));
        Ok(self.metadata_uri(format.metadata(), track_id, limit))
    }

    fn metadata_uri(
        &self,
        mut metadata: Metadata<'_>,
        track_id: Option<u64>,
        limit: usize,
    ) -> Option<Arc<ArtworkFile>> {
        let mut fallback = None;
        while let Some(revision) = metadata.current() {
            let visuals = revision.media.visuals.iter().chain(
                revision
                    .per_track
                    .iter()
                    .filter(|track| Some(track.track_id) == track_id)
                    .flat_map(|track| &track.metadata.visuals),
            );
            for visual in visuals {
                let front_cover = visual.usage == Some(StandardVisualKey::FrontCover);
                if (front_cover || fallback.is_none())
                    && let Some(uri) = self.image_uri(&visual.data, limit)
                {
                    if front_cover {
                        return Some(uri);
                    }
                    fallback = Some(uri);
                }
            }
            if metadata.pop().is_none() {
                break;
            }
        }
        fallback
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
        let path = cache::original_path(&self.directory.join("cache"), hash, extension);
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

fn managed_filename(name: &std::ffi::OsStr, bucket: bool) -> bool {
    let Some((stem, extension)) = name.to_str().and_then(|name| name.rsplit_once('.')) else {
        return false;
    };
    let hash = if bucket {
        if let Some((hash, tier)) = stem.split_once('-') {
            if extension != "webp" || !matches!(tier, "256" | "512" | "1024") {
                return false;
            }
            hash
        } else {
            stem
        }
    } else if let Some((hash, thumbnail)) = stem.split_once("-thumb-") {
        // Legacy root files are inventory/cleanup only, never a read fallback.
        let mut parts = thumbnail.split('-');
        let components = (parts.next(), parts.next(), parts.next(), parts.next());
        if extension != "webp"
            || !matches!(components, (Some(version), Some(quality), Some("256" | "512" | "1024"), None)
                if version.strip_prefix('v').is_some_and(decimal)
                    && (quality == "lossless" || quality.strip_prefix('q').is_some_and(decimal)))
        {
            return false;
        }
        hash
    } else {
        stem
    };
    lowercase_hex(hash, if bucket { 30 } else { 64 })
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

fn decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
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
    let file = rivu_format::input::open_regular_file(path)?;
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs::File;

    const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

    #[test]
    fn originals_use_bucketed_128_bit_names_without_reusing_or_migrating_legacy_files() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let root = directory.path().join("artwork/cache");
        fs::create_dir_all(&root).unwrap();
        let hash = blake3::hash(DEFAULT_IMAGE).to_hex();
        let legacy = root.join(format!("{hash}.png"));
        fs::write(&legacy, DEFAULT_IMAGE).unwrap();
        let image = manager.image_uri(DEFAULT_IMAGE, MAX_IMAGE_BYTES).unwrap();
        let expected = root.join(&hash[..2]).join(format!("{}.png", &hash[2..32]));
        assert_eq!(image.path.as_ref(), expected.as_path());
        assert_eq!(
            manager.export_default().unwrap().path().as_ref(),
            expected.as_path()
        );
        assert_eq!(fs::read(&expected).unwrap(), DEFAULT_IMAGE);
        assert_eq!(fs::read(&legacy).unwrap(), DEFAULT_IMAGE);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        assert_eq!(fs::read_dir(expected.parent().unwrap()).unwrap().count(), 1);
        // Removing the new original cannot turn the legacy copy into a fallback.
        fs::remove_file(&expected).unwrap();
        fs::create_dir(&expected).unwrap();
        assert!(
            ArtworkManager::new(directory.path())
                .export_default()
                .is_none()
        );
        assert_eq!(fs::read(legacy).unwrap(), DEFAULT_IMAGE);
    }

    #[test]
    fn existing_originals_require_bounded_content_equality_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let path = cache::original_path(
            &directory.path().join("artwork/cache"),
            blake3::hash(DEFAULT_IMAGE),
            "png",
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut collision = DEFAULT_IMAGE.to_vec();
        *collision.last_mut().unwrap() ^= 1;
        fs::write(&path, &collision).unwrap();
        assert_eq!(
            write_image(&path, DEFAULT_IMAGE).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(manager.image_uri(DEFAULT_IMAGE, MAX_IMAGE_BYTES).is_none());
        assert!(manager.export_default().is_none());
        assert_eq!(fs::read(&path).unwrap(), collision);
        // A length mismatch is rejected before reading any existing content.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert_eq!(
            write_image(&path, DEFAULT_IMAGE).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            MAX_IMAGE_BYTES as u64 + 1
        );
        fs::write(&path, DEFAULT_IMAGE).unwrap();
        age_file(&path);
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(file_path(&write_image(&path, DEFAULT_IMAGE).unwrap()), path);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn racing_different_publishers_never_clobber_the_winner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("collision.png");
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let left = scope.spawn(|| {
                barrier.wait();
                write_image(&path, b"left")
            });
            let right = scope.spawn(|| {
                barrier.wait();
                write_image(&path, b"rght")
            });
            [left.join().unwrap(), right.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = if results[0].is_ok() { b"left" } else { b"rght" };
        assert_eq!(fs::read(&path).unwrap(), winner);
        assert_eq!(
            results.into_iter().find_map(Result::err).unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn independent_managers_racing_identical_publications_share_a_complete_original() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = std::sync::Barrier::new(4);
        let images = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let manager = ArtworkManager::new(directory.path());
                        barrier.wait();
                        manager.export_default().unwrap()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        for image in &images {
            assert_eq!(image.uri(), images[0].uri());
            assert_eq!(fs::read(image.path()).unwrap(), DEFAULT_IMAGE);
        }
        assert_eq!(
            fs::read_dir(images[0].path().parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn original_publication_never_reuses_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("outside.png");
        fs::write(&target, DEFAULT_IMAGE).unwrap();
        let path = directory.path().join("cached.png");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            write_image(&path, DEFAULT_IMAGE).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).unwrap(), DEFAULT_IMAGE);
    }

    #[test]
    fn startup_and_negative_lookups_never_export_default_artwork() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let manager = ArtworkManager::new(&data);
        let selected = track(directory.path().join("missing.wav"));
        assert!(manager.cached(&selected).is_none());
        assert!(manager.cached_default().is_none());
        assert!(!data.exists());
        assert!(manager.load(&selected).is_none());
        assert!(manager.is_loaded(&selected));
        assert!(
            manager
                .load_with(&selected, || panic!("negative lookup reprobed"))
                .is_none()
        );
        assert!(!data.exists());
        let exported = manager.export_default().unwrap();
        assert_eq!(fs::read(file_path(exported.uri())).unwrap(), DEFAULT_IMAGE);
        assert!(manager.cached(&selected).is_none());
        assert!(
            manager
                .load_with(&selected, || panic!("export changed negative lookup"))
                .is_none()
        );
        assert!(!data.join("artwork/default.png").exists());
    }

    #[test]
    fn default_export_is_singleflight_and_protected_without_track_lookups() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let barrier = std::sync::Barrier::new(4);
        let held = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        manager.export_default().unwrap()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(held.iter().all(|image| image == &held[0]));
        let path = file_path(held[0].uri());
        assert!(managed_filename(path.file_name().unwrap(), true));
        assert_eq!(manager.cached_default(), Some(held[0].clone()));
        age_file(&path);
        let report = manager.maintain(Duration::ZERO, true).unwrap();
        assert_eq!(
            (report.files, report.protected_files, report.removed_files),
            (1, 1, 0)
        );
        assert_eq!(fs::read(&path).unwrap(), DEFAULT_IMAGE);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn failed_default_export_is_remembered_after_filesystem_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("data");
        fs::write(&blocked, b"not a directory").unwrap();
        let manager = ArtworkManager::new(&blocked);
        assert!(manager.cached_default().is_none());
        assert!(manager.export_default().is_none());
        fs::remove_file(&blocked).unwrap();
        fs::create_dir(&blocked).unwrap();
        assert!(manager.export_default().is_none());
        assert!(manager.cached_default().is_none());
        assert!(!blocked.join("artwork").exists());
    }

    #[test]
    fn configured_encoded_limit_applies_to_embedded_and_directory_images() {
        let directory = tempfile::tempdir().unwrap();
        for (version, size) in [(2, 9), (3, 16), (4, 17)] {
            let source = directory.path().join(format!("large-{version}.wav"));
            let size = size * 1024 * 1024;
            budget_tagged_wav(&source, version, &[(DEFAULT_IMAGE, size, 3)]);
            let selected = track(source);
            let default = ArtworkManager::new(directory.path());
            let result = default.load(&selected);
            assert_eq!(result.is_some(), size <= DEFAULT_MAX_IMAGE_BYTES);
            let small = ArtworkManager::new(directory.path()).with_max_image_bytes(1024 * 1024);
            assert!(small.load(&selected).is_none());
            let large =
                ArtworkManager::new(directory.path()).with_max_image_bytes(64 * 1024 * 1024);
            let image = large.load(&selected).unwrap();
            assert_eq!(
                fs::metadata(file_path(image.uri())).unwrap().len(),
                size as u64
            );
        }
        let cover = directory.path().join("cover.png");
        fs::write(&cover, DEFAULT_IMAGE).unwrap();
        File::options()
            .write(true)
            .open(&cover)
            .unwrap()
            .set_len(DEFAULT_MAX_IMAGE_BYTES as u64)
            .unwrap();
        let selected = track(directory.path().join("missing.wav"));
        let default = ArtworkManager::new(directory.path());
        assert_eq!(
            fs::metadata(file_path(default.load(&selected).unwrap().uri()))
                .unwrap()
                .len(),
            DEFAULT_MAX_IMAGE_BYTES as u64
        );
        let small = ArtworkManager::new(directory.path()).with_max_image_bytes(1024 * 1024);
        assert!(small.load(&selected).is_none());
        File::options()
            .write(true)
            .open(&cover)
            .unwrap()
            .set_len(DEFAULT_MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(
            ArtworkManager::new(directory.path())
                .load(&selected)
                .is_none()
        );
        let large = ArtworkManager::new(directory.path()).with_max_image_bytes(64 * 1024 * 1024);
        assert!(large.load(&selected).is_some());
    }

    fn assert_budget_audio(path: &Path) {
        use rivu_audio::{ProbeBackend, SourceRenderer};
        use rivu_format::media::DecoderHint;
        use std::sync::atomic::AtomicBool;

        let details = rivu_audio::probe_details(path, false, false).unwrap();
        assert!(matches!(details.decoder_backend, ProbeBackend::Native));
        assert_eq!(details.info.sample_rate, 8_000);
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("decoded.wav");
        let rendered = SourceRenderer::open(
            path,
            None,
            DecoderHint::Native,
            2,
            false,
            &AtomicBool::new(false),
        )
        .unwrap()
        .render(&output, u64::MAX, &AtomicBool::new(false))
        .unwrap();
        assert_eq!(rendered.frames, 1);
        let samples = hound::WavReader::open(output)
            .unwrap()
            .into_samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(samples, [0.5]);
    }

    #[test]
    fn native_wave_info_metadata_preserves_external_revision_order() {
        let directory = tempfile::tempdir().unwrap();
        for external in [false, true] {
            let path = directory.path().join(format!("info-{external}.wav"));
            let mut bytes = Vec::new();
            if external {
                let text = b"\0External title\0";
                let mut frame = b"TIT2".to_vec();
                frame.extend_from_slice(&(text.len() as u32).to_be_bytes());
                frame.extend_from_slice(&[0, 0]);
                frame.extend_from_slice(text);
                bytes.extend_from_slice(b"ID3\x03\0\0");
                bytes.extend_from_slice(&syncsafe(frame.len()));
                bytes.extend_from_slice(&frame);
            }
            let mut info = b"INFO".to_vec();
            for (key, value) in [
                (b"INAM", b"Info title\0".as_slice()),
                (b"IART", b"Info artist\0".as_slice()),
            ] {
                info.extend_from_slice(key);
                info.extend_from_slice(&(value.len() as u32).to_le_bytes());
                info.extend_from_slice(value);
                if value.len() % 2 != 0 {
                    info.push(0);
                }
            }
            let empty_info = b"LIST\x04\0\0\0INFO";
            bytes.extend_from_slice(b"RIFF");
            bytes.extend_from_slice(
                &(46 + info.len() as u32 + 2 * empty_info.len() as u32).to_le_bytes(),
            );
            bytes.extend_from_slice(
                b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0",
            );
            bytes.extend_from_slice(empty_info);
            bytes.extend_from_slice(b"LIST");
            bytes.extend_from_slice(&(info.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&info);
            bytes.extend_from_slice(empty_info);
            bytes.extend_from_slice(b"data\x02\0\0\0\0\x40");
            fs::write(&path, bytes).unwrap();

            let mut budget = rivu_audio::metadata_budget(false);
            let mut format = budget
                .with(|| {
                    get_probe().probe(
                        &Hint::new(),
                        MediaSourceStream::new(
                            Box::new(File::open(&path).unwrap()),
                            MediaSourceStreamOptions::default(),
                        ),
                        FormatOptions::default(),
                        MetadataOptions::default(),
                    )
                })
                .unwrap();
            let mut metadata = format.metadata();
            if external {
                let revision = metadata.pop().expect("external revision precedes INFO");
                assert_eq!(revision.media.tags.len(), 1);
                assert_eq!(
                    revision.media.tags[0].raw.value.to_string(),
                    "External title"
                );
            }
            let revision = metadata.current().expect("INFO revision survives");
            assert_eq!(revision.media.tags.len(), 2);
            assert_eq!(revision.media.tags[0].raw.key, "INAM");
            assert_eq!(
                revision.media.tags[0]
                    .raw
                    .value
                    .to_string()
                    .trim_end_matches('\0'),
                "Info title"
            );
            assert_eq!(revision.media.tags[1].raw.key, "IART");
            assert_eq!(
                revision.media.tags[1]
                    .raw
                    .value
                    .to_string()
                    .trim_end_matches('\0'),
                "Info artist"
            );
            assert!(metadata.is_latest());
            assert_eq!(format.next_packet().unwrap().unwrap().data.len(), 2);
        }
    }

    #[test]
    fn native_metadata_budget_preserves_unsynchronised_audio() {
        let directory = tempfile::tempdir().unwrap();
        for (version, tag_wide) in [(2, true), (3, true), (4, false), (4, true)] {
            let path = directory
                .path()
                .join(format!("unsync-{version}-{tag_wide}.wav"));
            budget_unsynchronised_wav(&path, version, tag_wide);
            assert_budget_audio(&path);
        }
    }

    #[test]
    fn native_metadata_budget_skips_large_pictures_without_losing_tags_or_audio() {
        let directory = tempfile::tempdir().unwrap();
        for version in [2, 3, 4] {
            let path = directory.path().join(format!("budget-{version}.wav"));
            let size = if version == 2 { 9 } else { 27 } * 1024 * 1024;
            budget_tagged_wav(
                &path,
                version,
                &[
                    (DEFAULT_IMAGE, size, 3),
                    (DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3),
                ],
            );
            assert_eq!(
                rivu_audio::probe(&path, false).unwrap().title,
                "Budget title"
            );
            assert_budget_audio(&path);
        }
    }

    #[test]
    fn native_metadata_budget_keeps_audio_after_many_picture_frames() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("many-pictures.wav");
        let pictures = vec![(DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3); 64];
        budget_tagged_wav(&path, 4, &pictures);
        assert_eq!(
            rivu_audio::probe(&path, false).unwrap().title,
            "Budget title"
        );
        assert_budget_audio(&path);
    }

    pub(crate) fn track(path: PathBuf) -> Track {
        Track {
            id: 1,
            path,
            fingerprint: Some("original".into()),
            cue: None,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: "wav".into(),
            channels: 1,
            sample_rate: 8_000,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: Some(16),
            release_date: None,
            favorite: false,
            decoder_hint: Default::default(),
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    pub(crate) fn tagged_wav(path: &Path) {
        tagged_wav_image(path, DEFAULT_IMAGE);
    }

    fn tagged_wav_image(path: &Path, image: &[u8]) {
        // ID3v2.3 APIC followed by one mono PCM WAV sample. Probing metadata
        // requires neither a decoder nor an audio device.
        let mut picture = b"\0image/png\0\x03\0".to_vec();
        picture.extend_from_slice(image);
        let mut frame = b"APIC".to_vec();
        frame.extend_from_slice(&(picture.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&picture);
        let size = frame.len() as u32;
        let mut bytes = b"ID3\x03\0\0".to_vec();
        for shift in [21, 14, 7, 0] {
            bytes.push(((size >> shift) & 0x7f) as u8);
        }
        bytes.extend_from_slice(&frame);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&38_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&0_i16.to_le_bytes());
        fs::write(path, bytes).unwrap();
    }

    fn syncsafe(value: usize) -> [u8; 4] {
        std::array::from_fn(|index| ((value >> (21 - 7 * index)) & 0x7f) as u8)
    }

    fn write_test_wave(file: &mut File) {
        use std::io::Write;
        file.write_all(b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x02\0\0\0\0\x40").unwrap();
    }

    // Stream large fixture payloads without creating large test buffers.
    pub(crate) fn budget_tagged_wav(path: &Path, version: u8, pictures: &[(&[u8], usize, u8)]) {
        use std::io::{Read, Write};
        let header_len = if version == 2 { 6 } else { 10 };
        let picture_prefix = if version == 2 { 6 } else { 13 };
        let title = b"\0Budget title";
        let tag_len = pictures
            .iter()
            .map(|(_, size, _)| header_len + picture_prefix + size)
            .sum::<usize>()
            + header_len
            + title.len();
        let mut file = File::create(path).unwrap();
        file.write_all(&[b'I', b'D', b'3', version, 0, 0]).unwrap();
        file.write_all(&syncsafe(tag_len)).unwrap();
        let frame_header = |file: &mut File, legacy: &[u8; 3], modern: &[u8; 4], size: usize| {
            if version == 2 {
                file.write_all(legacy).unwrap();
                file.write_all(&(size as u32).to_be_bytes()[1..]).unwrap();
            } else {
                file.write_all(modern).unwrap();
                file.write_all(&if version == 4 {
                    syncsafe(size)
                } else {
                    (size as u32).to_be_bytes()
                })
                .unwrap();
                file.write_all(&[0, 0]).unwrap();
            }
        };
        for &(bytes, size, usage) in pictures {
            assert!(size >= bytes.len());
            frame_header(&mut file, b"PIC", b"APIC", picture_prefix + size);
            if version == 2 {
                file.write_all(b"\0PNG").unwrap();
            } else {
                file.write_all(b"\0image/png\0").unwrap();
            }
            file.write_all(&[usage, 0]).unwrap();
            file.write_all(bytes).unwrap();
            std::io::copy(
                &mut std::io::repeat(0).take((size - bytes.len()) as u64),
                &mut file,
            )
            .unwrap();
        }
        frame_header(&mut file, b"TT2", b"TIT2", title.len());
        file.write_all(title).unwrap();
        write_test_wave(&mut file);
    }

    pub(crate) fn budget_unsynchronised_wav(path: &Path, version: u8, tag_wide: bool) {
        use std::io::Write;
        fn unsynchronise(bytes: &[u8]) -> Vec<u8> {
            let mut encoded = Vec::new();
            for &byte in bytes {
                encoded.push(byte);
                if byte == 0xff {
                    encoded.push(0);
                }
            }
            encoded
        }
        let mut picture = if version == 2 {
            b"\0PNG\x03\0".to_vec()
        } else {
            b"\0image/png\0\x03\0".to_vec()
        };
        picture.extend_from_slice(DEFAULT_IMAGE);
        let picture = if version == 4 {
            unsynchronise(&picture)
        } else {
            picture
        };
        let mut body = if version == 2 {
            b"PIC".to_vec()
        } else {
            b"APIC".to_vec()
        };
        if version == 2 {
            body.extend_from_slice(&(picture.len() as u32).to_be_bytes()[1..]);
        } else {
            body.extend_from_slice(&if version == 4 {
                syncsafe(picture.len())
            } else {
                (picture.len() as u32).to_be_bytes()
            });
            body.extend_from_slice(&[0, if version == 4 && !tag_wide { 2 } else { 0 }]);
        }
        body.extend_from_slice(&picture);
        let body = if version < 4 {
            unsynchronise(&body)
        } else {
            body
        };
        let mut file = File::create(path).unwrap();
        file.write_all(&[
            b'I',
            b'D',
            b'3',
            version,
            0,
            if version < 4 || tag_wide { 0x80 } else { 0 },
        ])
        .unwrap();
        file.write_all(&syncsafe(body.len())).unwrap();
        file.write_all(&body).unwrap();
        write_test_wave(&mut file);
    }

    #[test]
    fn metadata_budget_unsynchronised_covers_preserve_original_bytes() {
        let directory = tempfile::tempdir().unwrap();
        for (version, tag_wide) in [(2, true), (3, true), (4, false), (4, true)] {
            let path = directory
                .path()
                .join(format!("unsync-{version}-{tag_wide}.wav"));
            budget_unsynchronised_wav(&path, version, tag_wide);
            let manager = ArtworkManager::new(directory.path());
            let artwork = manager.load(&track(path)).unwrap();
            assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), DEFAULT_IMAGE);
        }
    }

    #[test]
    fn metadata_budget_many_tiny_pictures_cannot_bypass_object_quota() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tiny-pictures.wav");
        let mut bmp = [0_u8; 58];
        bmp[..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&58_u32.to_le_bytes());
        bmp[10..14].copy_from_slice(&54_u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40_u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&1_u32.to_le_bytes());
        bmp[22..26].copy_from_slice(&1_u32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1_u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24_u16.to_le_bytes());
        bmp[34..38].copy_from_slice(&4_u32.to_le_bytes());
        let mut pictures = vec![(&bmp[..], bmp.len(), 0); 2000];
        pictures.push((DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3));
        budget_tagged_wav(&path, 3, &pictures);
        let manager = ArtworkManager::new(directory.path());
        let artwork = manager.load(&track(path)).unwrap();
        assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), bmp);
    }

    #[test]
    fn metadata_budget_keeps_small_and_boundary_covers_across_id3_versions() {
        let directory = tempfile::tempdir().unwrap();
        for version in [2, 3, 4] {
            for size in [DEFAULT_IMAGE.len(), MAX_IMAGE_BYTES] {
                let path = directory.path().join(format!("small-{version}-{size}.wav"));
                budget_tagged_wav(&path, version, &[(DEFAULT_IMAGE, size, 3)]);
                let manager =
                    ArtworkManager::new(directory.path()).with_max_image_bytes(MAX_IMAGE_BYTES);
                let artwork = manager.load(&track(path)).unwrap();
                let retained = fs::read(file_path(artwork.uri())).unwrap();
                assert_eq!(retained.len(), size);
                assert!(retained.starts_with(DEFAULT_IMAGE));
            }
        }
    }

    #[test]
    fn metadata_budget_skips_oversized_cover_and_keeps_following_cover() {
        let directory = tempfile::tempdir().unwrap();
        for version in [2, 3, 4] {
            for size in [
                MAX_IMAGE_BYTES + 1,
                if version == 2 { 9 } else { 27 } * 1024 * 1024,
            ] {
                let path = directory
                    .path()
                    .join(format!("oversized-{version}-{size}.wav"));
                budget_tagged_wav(
                    &path,
                    version,
                    &[
                        (DEFAULT_IMAGE, size, 3),
                        (DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3),
                    ],
                );
                let manager =
                    ArtworkManager::new(directory.path()).with_max_image_bytes(MAX_IMAGE_BYTES);
                let artwork = manager.load(&track(path)).unwrap();
                assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), DEFAULT_IMAGE);
            }
        }
    }

    #[test]
    fn metadata_budget_caps_aggregate_covers_without_changing_fallback_choice() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aggregate.wav");
        budget_tagged_wav(
            &path,
            4,
            &[
                (DEFAULT_IMAGE, MAX_IMAGE_BYTES, 0),
                (DEFAULT_IMAGE, MAX_IMAGE_BYTES, 0),
                (DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3),
            ],
        );
        let manager = ArtworkManager::new(directory.path()).with_max_image_bytes(MAX_IMAGE_BYTES);
        let artwork = manager.load(&track(path)).unwrap();
        assert_eq!(
            fs::metadata(file_path(artwork.uri())).unwrap().len(),
            MAX_IMAGE_BYTES as u64
        );
    }

    #[test]
    fn metadata_budget_oversized_only_has_no_real_cover() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("only-oversized.wav");
        budget_tagged_wav(&path, 3, &[(DEFAULT_IMAGE, 27 * 1024 * 1024, 3)]);
        let manager = ArtworkManager::new(directory.path());
        let selected = track(path);
        assert!(manager.load(&selected).is_none());
        assert!(manager.is_loaded(&selected));
    }

    fn file_path(uri: &str) -> PathBuf {
        url::Url::parse(uri).unwrap().to_file_path().unwrap()
    }

    fn result_uri(result: Option<Artwork>) -> Option<String> {
        result.map(|artwork| artwork.uri().to_owned())
    }

    fn age_file(path: &Path) {
        File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(172_800))
            .unwrap();
    }

    #[test]
    fn embedded_picture_is_deduplicated_and_metadata_edits_do_not_reload() {
        let directory = tempfile::Builder::new()
            .prefix("rivu artwork #")
            .tempdir()
            .unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let mut selected = track(source);
        assert!(manager.cached(&selected).is_none());
        let first = manager.load(&selected).unwrap();
        let path = file_path(first.uri());
        assert_eq!(fs::read(&path).unwrap(), DEFAULT_IMAGE);
        assert!(first.uri().contains("%23"));
        selected.id = 2;
        let second = manager.load(&selected).unwrap();
        assert_eq!(first.uri(), second.uri());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        selected.path = directory.path().join("missing.wav");
        selected.title = "Edited title".into();
        assert_eq!(manager.load(&selected), Some(second));
        selected.fingerprint = Some("changed".into());
        assert!(manager.load(&selected).is_none());
        assert!(manager.cached(&selected).is_none());
    }

    #[test]
    fn failed_source_paths_retry_after_relocation_without_changing_content_identity() {
        let directory = tempfile::tempdir().unwrap();
        let cover_source = directory.path().join("available.wav");
        tagged_wav(&cover_source);
        for old_path in ["missing.wav", "unprobeable.wav"] {
            let manager = ArtworkManager::new(directory.path());
            let mut selected = track(directory.path().join(old_path));
            if old_path == "unprobeable.wav" {
                fs::write(&selected.path, b"not an audio file").unwrap();
            }
            assert!(manager.load(&selected).is_none());
            assert!(manager.is_loaded(&selected));
            assert_eq!(
                manager.load_with(&selected, || panic!("unchanged failure reprobed")),
                None
            );

            selected.path = cover_source.clone();
            assert!(!manager.is_loaded(&selected));
            assert!(manager.cached(&selected).is_none());
            let recovered = manager.load(&selected).unwrap();
            assert_eq!(fs::read(file_path(recovered.uri())).unwrap(), DEFAULT_IMAGE);
            selected.path = directory.path().join("moved-again.wav");
            assert_eq!(
                manager.load_with(&selected, || panic!("successful content reprobed")),
                Some(recovered)
            );
        }
    }

    #[test]
    fn no_cover_result_retries_directory_after_relocation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("no-cover.wav");
        tagged_wav_image(&source, b"unsupported image bytes");
        let manager = ArtworkManager::new(directory.path());
        let mut selected = track(source);
        assert!(manager.load(&selected).is_none());
        assert!(manager.is_loaded(&selected));
        assert_eq!(
            manager.load_with(&selected, || panic!("unchanged negative reprobed")),
            None
        );
        let moved = directory.path().join("moved");
        fs::create_dir(&moved).unwrap();
        selected.path = moved.join("track.wav");
        write_test_wave(&mut File::create(&selected.path).unwrap());
        fs::write(moved.join("cover.png"), DEFAULT_IMAGE).unwrap();
        assert!(!manager.is_loaded(&selected));
        assert!(manager.cached(&selected).is_none());
        let recovered = manager.load(&selected).unwrap();
        assert_eq!(fs::read(file_path(recovered.uri())).unwrap(), DEFAULT_IMAGE);
    }

    #[cfg(unix)]
    #[test]
    fn directory_fifo_and_fifo_symlink_do_not_block_following_image() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("cover.png");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path, and mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink(&fifo, directory.path().join("folder.png")).unwrap();
        fs::write(directory.path().join("front.png"), DEFAULT_IMAGE).unwrap();
        let selected = track(directory.path().join("track.wav"));
        let manager = ArtworkManager::new(directory.path());
        let (complete, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let artwork = manager.load(&selected).unwrap();
            complete
                .send(fs::read(file_path(artwork.uri())).unwrap())
                .unwrap();
        });
        // A broken nonblocking-open contract must fail rather than hang the suite.
        let bytes = completed.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert_eq!(bytes, DEFAULT_IMAGE);
    }

    fn directory_image(path: &Path, marker: u8) -> Vec<u8> {
        let mut bytes = DEFAULT_IMAGE.to_vec();
        bytes.push(marker);
        fs::write(path, &bytes).unwrap();
        bytes
    }

    #[test]
    fn directory_images_follow_embedded_covers_and_deterministic_filename_priority() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        let selected = track(source.clone());
        let candidates = [
            "cover.jpg",
            "COVER.PNG",
            "cover.png",
            "Folder.PNG",
            "front.webp",
        ];
        let images: Vec<_> = candidates
            .iter()
            .enumerate()
            .map(|(index, name)| directory_image(&directory.path().join(name), index as u8))
            .collect();
        tagged_wav(&source);
        let embedded = ArtworkManager::new(directory.path())
            .load(&selected)
            .unwrap();
        assert_eq!(fs::read(file_path(embedded.uri())).unwrap(), DEFAULT_IMAGE);
        write_test_wave(&mut File::create(&source).unwrap());
        for (name, expected) in candidates.iter().zip(&images) {
            // Each manager has a fresh session lookup; old immutable disk copies
            // must not become candidates or affect the chosen source image.
            let manager = ArtworkManager::new(directory.path());
            let image = manager.load(&selected).unwrap();
            assert_eq!(fs::read(file_path(image.uri())).unwrap(), *expected);
            assert_eq!(file_path(image.uri()).extension().unwrap(), "png");
            fs::remove_file(directory.path().join(name)).unwrap();
        }
        assert!(
            ArtworkManager::new(directory.path())
                .load(&selected)
                .is_none()
        );
    }

    #[test]
    fn directory_images_require_exact_stems_and_supported_extensions() {
        let directory = tempfile::tempdir().unwrap();
        let selected = track(directory.path().join("missing.wav"));
        for name in [
            "other.png",
            "cover.back.jpg",
            "cover-small.png",
            "cover.svg",
            "folder.avif",
            "front",
            "cover.png.bak",
        ] {
            directory_image(&directory.path().join(name), 1);
        }
        assert!(
            ArtworkManager::new(directory.path())
                .load(&selected)
                .is_none()
        );
        for extension in [
            "PNG", "JpG", "JPEG", "GIF", "WebP", "BMP", "TIF", "TIFF", "ICO",
        ] {
            let path = directory.path().join(format!("FrOnT.{extension}"));
            let expected = directory_image(&path, 2);
            let image = ArtworkManager::new(directory.path())
                .load(&selected)
                .unwrap();
            assert_eq!(fs::read(file_path(image.uri())).unwrap(), expected);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn directory_fallback_skips_invalid_oversized_and_nested_images() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        write_test_wave(&mut File::create(&source).unwrap());
        let cover = directory.path().join("cover.png");
        fs::write(&cover, DEFAULT_IMAGE).unwrap();
        File::options()
            .write(true)
            .open(&cover)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        fs::write(directory.path().join("folder.png"), b"not an image").unwrap();
        let nested = directory.path().join("front.png");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("cover.png"), DEFAULT_IMAGE).unwrap();
        let boundary = directory.path().join("front.TIFF");
        fs::write(&boundary, DEFAULT_IMAGE).unwrap();
        File::options()
            .write(true)
            .open(&boundary)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64)
            .unwrap();
        let selected = track(source);
        let manager = ArtworkManager::new(directory.path()).with_max_image_bytes(MAX_IMAGE_BYTES);
        let accepted = manager.load(&selected).unwrap();
        assert_eq!(
            fs::metadata(file_path(accepted.uri())).unwrap().len(),
            MAX_IMAGE_BYTES as u64
        );
        fs::remove_file(boundary).unwrap();
        assert!(
            ArtworkManager::new(directory.path())
                .with_max_image_bytes(MAX_IMAGE_BYTES)
                .load(&selected)
                .is_none()
        );
    }

    #[test]
    fn directory_images_are_path_bound_and_cached_as_immutable_content() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let old_bytes = directory_image(&first.join("cover.png"), 1);
        let new_bytes = directory_image(&second.join("cover.png"), 2);
        let mut selected = track(first.join("missing.wav"));
        let manager = ArtworkManager::new(directory.path());
        let original_identity = ArtworkIdentity::for_track(&selected);
        // A missing or unprobeable audio file does not hide its directory cover.
        let old = manager.load(&selected).unwrap();
        fs::remove_file(first.join("cover.png")).unwrap();
        assert_eq!(fs::read(file_path(old.uri())).unwrap(), old_bytes);
        selected.path = second.join("unprobeable.wav");
        fs::write(&selected.path, b"not audio").unwrap();
        assert!(!original_identity.matches(&selected));
        assert!(!manager.is_loaded(&selected));
        assert!(manager.cached(&selected).is_none());
        let new = manager.load(&selected).unwrap();
        assert_ne!(new.uri(), old.uri());
        assert_eq!(fs::read(file_path(new.uri())).unwrap(), new_bytes);
        assert_eq!(fs::read(file_path(old.uri())).unwrap(), old_bytes);
        selected.title = "Metadata edit".into();
        assert_eq!(
            manager.load_with(&selected, || panic!("metadata edit reprobed")),
            Some(new)
        );
    }

    #[test]
    fn unfinished_directory_lookup_cannot_complete_a_relocated_request() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let old_bytes = directory_image(&first.join("cover.png"), 1);
        let new_bytes = directory_image(&second.join("cover.png"), 2);
        let original = track(first.join("track.wav"));
        let mut relocated = original.clone();
        relocated.path = second.join("track.wav");
        let manager = ArtworkManager::new(directory.path());
        let (started, loading) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let manager = &manager;
            let original = &original;
            let old = scope.spawn(move || {
                manager.load_with(original, || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    manager.source_uri(&original.path, manager.max_image_bytes)
                })
            });
            loading.recv().unwrap();
            assert!(!manager.is_loaded(&relocated));
            assert!(manager.cached(&relocated).is_none());
            let new = scope.spawn(|| manager.load(&relocated).unwrap());
            release.send(()).unwrap();
            let old = old.join().unwrap().unwrap();
            let new = new.join().unwrap();
            assert_eq!(fs::read(file_path(old.uri())).unwrap(), old_bytes);
            assert_eq!(fs::read(file_path(new.uri())).unwrap(), new_bytes);
            assert_eq!(manager.cached(&relocated), Some(new));
        });
    }

    #[test]
    fn concurrent_requests_probe_once_and_share_a_complete_image() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let selected = track(source);
        let barrier = std::sync::Barrier::new(4);
        let probes = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        manager
                            .load_with(&selected, || {
                                probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                manager.source_uri(&selected.path, manager.max_image_bytes)
                            })
                            .unwrap()
                    })
                })
                .collect();
            for handle in handles {
                let artwork = handle.join().unwrap();
                assert_eq!(fs::read(file_path(artwork.uri())).unwrap(), DEFAULT_IMAGE);
                assert_eq!(manager.cached(&selected), Some(artwork));
            }
        });
        assert_eq!(probes.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_without_writer_falls_back_and_releases_worker_and_maintenance() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        let name = std::ffi::CString::new(source.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path, and mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let alias = directory.path().join("linked.wav");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        let manager = ArtworkManager::new(directory.path());
        let cache = directory.path().join("artwork/cache");
        fs::create_dir_all(&cache).unwrap();
        let expired = cache.join(format!("{}.png", "1".repeat(64)));
        fs::write(&expired, b"old").unwrap();
        let (complete, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut selected = track(source);
            let direct = result_uri(manager.load(&selected));
            assert!(manager.is_loaded(&selected));
            selected.id = 2;
            selected.path = alias;
            let linked = result_uri(manager.load(&selected));
            assert!(manager.is_loaded(&selected));
            let report = manager.maintain(Duration::ZERO, true).unwrap();
            complete
                .send((direct, linked, report.removed_files))
                .unwrap();
        });
        // Do not join on failure: a regression must fail rather than hang the suite.
        let (direct, linked, removed) = completed.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(direct.is_none());
        assert!(linked.is_none());
        assert_eq!(removed, 1);
        assert!(!expired.exists());
    }

    #[test]
    fn cache_only_reads_do_not_wait_for_probe_or_manual_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let selected = track(directory.path().join("missing.wav"));
        let (started, loading) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (read, reads) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let manager = &manager;
            let selected = &selected;
            let loader = scope.spawn(move || {
                manager.load_with(selected, || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    ArtworkSource::Directory(None)
                })
            });
            loading.recv().unwrap();
            scope.spawn(move || {
                read.send((manager.cached(selected), manager.is_loaded(selected)))
                    .unwrap()
            });
            let observed = reads.recv_timeout(Duration::from_secs(2));
            release.send(()).unwrap();
            assert_eq!(observed.unwrap(), (None, false));
            loader.join().unwrap();
        });
        let _cleaning = manager.maintenance.write();
        assert!(manager.is_loaded(&selected));
        assert!(manager.cached(&selected).is_none());
    }

    #[test]
    fn held_old_version_survives_pressure_and_late_completion_does_not_replace_new() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let old_track = track(source.clone());
        let mut current = old_track.clone();
        current.fingerprint = Some("new".into());
        let (started, loading) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (old, new) = std::thread::scope(|scope| {
            let manager = &manager;
            let old_track = &old_track;
            let source = &source;
            let loader = scope.spawn(move || {
                manager.load_with(old_track, || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    manager.source_uri(source, manager.max_image_bytes)
                })
            });
            loading.recv().unwrap();
            let new = manager.load(&current).unwrap();
            release.send(()).unwrap();
            (loader.join().unwrap().unwrap(), new)
        });
        assert_eq!(old.uri(), new.uri());
        assert_ne!(old, new);
        for id in 2..130 {
            let mut other = current.clone();
            other.id = id;
            other.path = directory.path().join("missing");
            manager.load(&other);
        }
        assert_eq!(manager.cached(&old_track), Some(old));
        assert_eq!(manager.cached(&current), Some(new));
        assert!(file_path(manager.cached(&current).unwrap().uri()).is_file());
    }

    #[test]
    fn held_results_can_exceed_target_and_idle_results_are_evicted() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let mut selected = track(source);
        let mut held = Vec::new();
        for id in 1..=MAX_ARTWORK_ENTRIES + 2 {
            selected.id = id as i64;
            held.push(manager.load(&selected).unwrap());
        }
        selected.id = 1;
        assert_eq!(manager.cached(&selected), Some(held[0].clone()));
        assert!(manager.is_loaded(&selected));
        drop(held);
        selected.id = 1000;
        manager.load(&selected);
        selected.id = 1;
        assert!(!manager.is_loaded(&selected));
        selected.id = (MAX_ARTWORK_ENTRIES + 2) as i64;
        assert!(manager.is_loaded(&selected));
    }

    #[test]
    fn manual_cleanup_respects_age_residency_and_reloads_evicted_content() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let selected = track(source);
        let held = manager.load(&selected).unwrap();
        let protected = file_path(held.uri());
        age_file(&protected);
        let cache = directory.path().join("artwork/cache");
        let expired = cache.join(format!("{}.png", "1".repeat(64)));
        let fresh = cache.join(format!("{}.png", "2".repeat(64)));
        fs::write(&expired, b"old").unwrap();
        fs::write(&fresh, b"fresh").unwrap();
        age_file(&expired);
        let unmanaged = cache.join("notes.txt");
        fs::write(&unmanaged, b"keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&unmanaged, cache.join(format!("{}.png", "3".repeat(64))))
            .unwrap();
        let age = Duration::from_secs(86_400);
        let report = manager.maintain(age, false).unwrap();
        assert_eq!(report.files, 3);
        assert_eq!(report.bytes, DEFAULT_IMAGE.len() as u64 + 8);
        assert_eq!(
            (
                report.expired_files,
                report.protected_files,
                report.removed_files
            ),
            (2, 1, 0)
        );
        let report = manager.maintain(age, true).unwrap();
        assert_eq!(
            (
                report.removed_files,
                report.removed_bytes,
                report.failed_files
            ),
            (1, 3, 0)
        );
        assert!(protected.is_file());
        assert!(fresh.is_file());
        assert!(unmanaged.is_file());
        assert!(!expired.exists());
        drop(held);
        for id in 2..130 {
            let mut other = selected.clone();
            other.id = id;
            other.path = directory.path().join("missing");
            manager.load(&other);
        }
        let report = manager.maintain(age, true).unwrap();
        assert_eq!(
            (report.removed_files, report.removed_bytes),
            (1, DEFAULT_IMAGE.len() as u64)
        );
        assert!(!protected.exists());
        assert!(!manager.is_loaded(&selected));
        let reloaded = manager.load(&selected).unwrap();
        assert_eq!(file_path(reloaded.uri()), protected);
        assert_eq!(fs::read(protected).unwrap(), DEFAULT_IMAGE);
    }

    #[test]
    fn thumbnail_cleanup_includes_both_layouts_without_pinning_to_resident_originals() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let original = manager.export_default().unwrap();
        let source = original.path();
        let bucket = source.parent().unwrap();
        let cache = bucket.parent().unwrap();
        let hash = blake3::hash(DEFAULT_IMAGE);
        let legacy = cache.join(format!("{hash}.png"));
        fs::write(&legacy, DEFAULT_IMAGE).unwrap();
        let thumbnails =
            [256, 512, 1024].map(|tier| cache::thumbnail_path(&source, hash, tier).unwrap());
        let old_thumbnails = ["v2-lossless-256", "v1-q80-512", "v0-q85-1024"]
            .map(|suffix| cache.join(format!("{hash}-thumb-{suffix}.webp")));
        for thumbnail in thumbnails.iter().chain(&old_thumbnails) {
            fs::write(thumbnail, b"thumbnail").unwrap();
        }
        for name in [
            format!("{hash}-thumb-v1-q80-128.webp"),
            format!("{hash}-thumb-v1-q80-256.png"),
            format!("{hash}-thumb-v-q80-256.webp"),
            format!("{hash}-thumb-v1-q-256.webp"),
            format!("{hash}-thumb-v1-q80-256-extra.webp"),
        ] {
            assert!(!managed_filename(std::ffi::OsStr::new(&name), false));
        }
        let report = manager.maintain(Duration::ZERO, true).unwrap();
        assert_eq!(report.files, 8);
        assert_eq!(report.protected_files, 1);
        assert_eq!(report.removed_files, 7);
        assert!(source.is_file());
        assert!(!legacy.exists());
        assert!(
            thumbnails
                .iter()
                .chain(&old_thumbnails)
                .all(|path| !path.exists())
        );
    }

    #[test]
    fn maintenance_excludes_unmanaged_names_symlinks_and_nested_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("artwork/cache");
        let bucket = root.join("ab");
        fs::create_dir_all(&bucket).unwrap();
        let short_hash = "1".repeat(30);
        let full_hash = "2".repeat(64);
        let mut excluded = vec![
            root.join(format!("{short_hash}.png")),
            root.join(format!("{short_hash}-256.webp")),
            bucket.join(format!("{full_hash}.png")),
            bucket.join(format!("{full_hash}-thumb-v2-lossless-256.webp")),
            bucket.join(format!("{}{}.png", "A", "1".repeat(29))),
            bucket.join(format!("{short_hash}-128.webp")),
            bucket.join(format!("{short_hash}-2048.webp")),
            bucket.join(format!("{short_hash}-0256.webp")),
            bucket.join(format!("{short_hash}-256.png")),
            bucket.join(format!("{short_hash}-256-extra.webp")),
            bucket.join(format!("{short_hash}-thumb-v2-lossless-256.webp")),
            bucket.join(format!("{short_hash}.PNG")),
            bucket.join("notes.txt"),
        ];
        for name in ["AB", "a", "abc", "g1", "unmanaged"] {
            let unmanaged = root.join(name);
            fs::create_dir_all(&unmanaged).unwrap();
            excluded.push(unmanaged.join(format!("{short_hash}.png")));
        }
        let nested = bucket.join("cd");
        fs::create_dir(&nested).unwrap();
        excluded.push(nested.join(format!("{short_hash}.png")));
        let named_directory = bucket.join(format!("{short_hash}.png"));
        fs::create_dir(&named_directory).unwrap();
        excluded.push(named_directory.join(format!("{short_hash}.png")));
        for path in &excluded {
            fs::write(path, b"keep").unwrap();
        }
        #[cfg(unix)]
        {
            let outside = directory.path().join("outside");
            fs::create_dir(&outside).unwrap();
            let external = outside.join(format!("{short_hash}.png"));
            fs::write(&external, b"keep").unwrap();
            excluded.push(external.clone());
            std::os::unix::fs::symlink(&outside, root.join("ef")).unwrap();
            std::os::unix::fs::symlink(&external, bucket.join(format!("{short_hash}-512.webp")))
                .unwrap();
            std::os::unix::fs::symlink(&external, root.join(format!("{full_hash}.png"))).unwrap();
        }
        let managed = bucket.join(format!("{short_hash}-256.webp"));
        fs::write(&managed, b"remove").unwrap();
        let manager = ArtworkManager::new(directory.path());
        let inventory = manager.maintain(Duration::ZERO, false).unwrap();
        assert_eq!((inventory.files, inventory.bytes), (1, 6));
        assert!(managed.is_file());
        let cleaned = manager.maintain(Duration::ZERO, true).unwrap();
        assert_eq!(
            (cleaned.files, cleaned.removed_files, cleaned.failed_files),
            (1, 1, 0)
        );
        assert!(!managed.exists());
        for path in excluded {
            assert_eq!(fs::read(path).unwrap(), b"keep");
        }
    }

    #[test]
    fn cleanup_waits_for_first_acquisition_and_preserves_newly_registered_file() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let selected = track(source);
        let cache = directory.path().join("artwork/cache");
        let path = cache::original_path(&cache, blake3::hash(DEFAULT_IMAGE), "png");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, DEFAULT_IMAGE).unwrap();
        age_file(&path);
        let (started, loading) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let manager = &manager;
            let selected = &selected;
            let loader = scope.spawn(move || {
                manager.load_with(selected, || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    manager.source_uri(&selected.path, manager.max_image_bytes)
                })
            });
            loading.recv().unwrap();
            let cleanup = scope.spawn(|| manager.maintain(Duration::from_secs(86_400), true));
            release.send(()).unwrap();
            let loaded = loader.join().unwrap().unwrap();
            let report = cleanup.join().unwrap().unwrap();
            assert_eq!(
                (
                    report.expired_files,
                    report.protected_files,
                    report.removed_files
                ),
                (1, 1, 0)
            );
            assert_eq!(fs::read(file_path(loaded.uri())).unwrap(), DEFAULT_IMAGE);
        });
    }

    #[test]
    fn failures_are_cached_and_manual_directory_errors_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let missing = track(directory.path().join("missing"));
        assert!(manager.load(&missing).is_none());
        assert!(
            manager
                .load_with(&missing, || panic!("cached failure reprobed"))
                .is_none()
        );
        assert!(manager.is_loaded(&missing));
        assert_eq!(manager.maintain(Duration::ZERO, false).unwrap().files, 0);
        fs::create_dir_all(directory.path().join("artwork")).unwrap();
        fs::write(directory.path().join("artwork/cache"), b"blocked").unwrap();
        assert!(manager.maintain(Duration::ZERO, false).is_err());
        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let unavailable = ArtworkManager::new(&blocked);
        assert_eq!(unavailable.load(&missing), None);
        assert!(unavailable.is_loaded(&missing));
    }
}
