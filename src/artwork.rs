//! Local, best-effort artwork shared by the application session. Cache-only
//! reads never probe media; blocking loads belong on existing background workers.
//! No decoder or audio output is opened; failures never propagate into playback.

use crate::model::Track;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};
use symphonia::{
    core::{
        formats::{FormatOptions, TrackType, probe::Hint},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::{Metadata, MetadataOptions, StandardVisualKey, Visual},
    },
    default::get_probe,
};

pub(crate) static DEFAULT_IMAGE: &[u8] = include_bytes!("../assets/rivu.png");

const MAX_ARTWORK_ENTRIES: usize = 64;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

struct ArtworkFile {
    uri: String,
    path: Arc<Path>,
    placeholder: bool,
}

/// Holds the lookup result that protects a published URI until it is released.
#[derive(Clone)]
pub struct Artwork(Arc<CachedArtwork>);

impl Artwork {
    pub fn uri(&self) -> &str {
        &self.file().uri
    }

    pub(crate) fn path(&self) -> Arc<Path> {
        Arc::clone(&self.file().path)
    }

    pub(crate) fn is_placeholder(&self) -> bool {
        self.file().placeholder
    }

    fn file(&self) -> &ArtworkFile {
        self.0
            .lookup
            .get()
            .and_then(|lookup| lookup.image.as_deref())
            .expect("published artwork")
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
}

impl ArtworkIdentity {
    pub(crate) fn for_track(track: &Track) -> Self {
        Self {
            id: track.id,
            fingerprint: track.fingerprint.clone(),
        }
    }

    pub(crate) fn matches(&self, track: &Track) -> bool {
        self.id == track.id && self.fingerprint == track.fingerprint
    }
}

struct CachedArtwork {
    identity: ArtworkIdentity,
    lookup: OnceLock<ArtworkLookup>,
}

struct ArtworkLookup {
    image: Option<Arc<ArtworkFile>>,
    failed_path: Option<PathBuf>,
}

impl ArtworkLookup {
    fn applies_to(&self, path: &Path) -> bool {
        self.failed_path
            .as_deref()
            .is_none_or(|failed_path| failed_path == path)
    }
}

impl CachedArtwork {
    fn matches(&self, track: &Track) -> bool {
        self.identity.matches(track)
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
                Arc::strong_count(entry) == 1 + usize::from(indexed)
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
    default: Option<Artwork>,
    // Background acquisition holds a reader through file creation and lookup
    // publication. Cleanup holds a writer only for one file at a time. Cache-
    // only reads never take this gate, and no lookup lock covers filesystem I/O.
    maintenance: RwLock<()>,
    entries: Mutex<ArtworkCache>,
}

impl ArtworkManager {
    pub fn new(data_dir: &Path) -> Self {
        let directory = data_dir.join("artwork");
        let default = write_image(&directory.join("default.png"), DEFAULT_IMAGE)
            .ok()
            .map(|uri| {
                let lookup = OnceLock::new();
                let _ = lookup.set(ArtworkLookup {
                    image: Some(artwork_file(uri, true)),
                    failed_path: None,
                });
                Artwork(Arc::new(CachedArtwork {
                    identity: ArtworkIdentity {
                        id: 0,
                        fingerprint: None,
                    },
                    lookup,
                }))
            });
        // Published file URIs are absolute; keep maintenance paths comparable
        // when --data-dir was supplied as a relative path.
        let directory = fs::canonicalize(&directory).unwrap_or(directory);
        Self {
            directory,
            default,
            maintenance: RwLock::new(()),
            entries: Mutex::new(ArtworkCache::default()),
        }
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
            .map(|entry| Artwork(Arc::clone(entry)))
            .or_else(|| self.default.clone())
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
        self.load_with(track, || self.embedded_uri(&track.path))
    }

    fn load_with(
        &self,
        track: &Track,
        probe: impl FnOnce() -> Result<Option<Arc<ArtworkFile>>, ()>,
    ) -> Option<Artwork> {
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
                let result = probe.take().expect("one source probe")();
                ArtworkLookup {
                    failed_path: result.is_err().then(|| track.path.clone()),
                    image: result.ok().flatten().or_else(|| {
                        self.default.as_ref().map(|artwork| {
                            Arc::clone(
                                artwork
                                    .0
                                    .lookup
                                    .get()
                                    .expect("default initialized")
                                    .image
                                    .as_ref()
                                    .expect("default image"),
                            )
                        })
                    }),
                }
            });
            // A concurrent request may have shared an unfinished old-path probe.
            // Only its failure is path-bound; successful content remains reusable.
            if !lookup.applies_to(&track.path) {
                continue;
            }
            let result = lookup.image.as_ref().map(|_| Artwork(Arc::clone(&cached)));
            self.entries.lock().trim();
            return result;
        }
    }

    /// Explicit disk inventory/cleanup. Startup and ordinary loads never enumerate
    /// the cache, and disk space has no automatic budget or session-wide index.
    pub fn maintain(&self, older_than: Duration, clean: bool) -> io::Result<ArtworkCacheReport> {
        let directory = self.directory.join("cache");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ArtworkCacheReport::default());
            }
            Err(error) => return Err(error),
        };
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
            if !managed_filename(&entry.file_name()) {
                continue;
            }
            // Serializes the first load's existence check/write/registration with
            // removal. Cached reads can still acquire any resident entry here.
            let _removal = clean.then(|| self.maintenance.write());
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => continue,
                Err(error) => {
                    report.failure(&path, error);
                    continue;
                }
            };
            report.files += 1;
            report.bytes += metadata.len();
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    report.failure(&path, error);
                    continue;
                }
            };
            if !now
                .duration_since(modified)
                .is_ok_and(|age| age >= older_than)
            {
                continue;
            }
            report.expired_files += 1;
            report.expired_bytes += metadata.len();
            if self.entries.lock().protects(&path) {
                report.protected_files += 1;
                report.protected_bytes += metadata.len();
                continue;
            }
            if clean {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        report.removed_files += 1;
                        report.removed_bytes += metadata.len();
                    }
                    Err(error) => report.failure(&path, error),
                }
            }
        }
        Ok(report)
    }

    fn embedded_uri(&self, path: &Path) -> Result<Option<Arc<ArtworkFile>>, ()> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // A stale or replaced media path may be a FIFO with no writer.
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path).map_err(|_| ())?;
        // Check the opened object, not the path, so replacement cannot turn a
        // metadata-approved regular file into a blocking stream before probing.
        if !file.metadata().map_err(|_| ())?.is_file() {
            return Err(());
        }
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let mut format = get_probe()
            .probe(
                &hint,
                MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default()),
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|_| ())?;
        let track_id = format
            .default_track(TrackType::Audio)
            .map(|track| u64::from(track.id));
        Ok(self.metadata_uri(format.metadata(), track_id))
    }

    fn metadata_uri(
        &self,
        mut metadata: Metadata<'_>,
        track_id: Option<u64>,
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
                    && let Some(uri) = self.visual_uri(visual)
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

    fn visual_uri(&self, visual: &Visual) -> Option<Arc<ArtworkFile>> {
        if visual.data.len() > MAX_IMAGE_BYTES {
            return None;
        }
        // Sniff the bytes rather than trusting a tag-supplied MIME type or name.
        // Keep the original image; desktop clients handle decoding and scaling.
        let extension = match image::guess_format(&visual.data).ok()? {
            image::ImageFormat::Png => "png",
            image::ImageFormat::Jpeg => "jpg",
            image::ImageFormat::Gif => "gif",
            image::ImageFormat::WebP => "webp",
            image::ImageFormat::Bmp => "bmp",
            image::ImageFormat::Tiff => "tiff",
            image::ImageFormat::Ico => "ico",
            _ => return None,
        };
        let hash = blake3::hash(&visual.data);
        let path = self
            .directory
            .join("cache")
            .join(format!("{hash}.{extension}"));
        let uri = write_image(&path, &visual.data).ok()?;
        Some(artwork_file(uri, false))
    }
}

fn artwork_file(uri: String, placeholder: bool) -> Arc<ArtworkFile> {
    let path = url::Url::parse(&uri)
        .expect("artwork file URI")
        .to_file_path()
        .expect("local artwork");
    Arc::new(ArtworkFile {
        uri,
        path: Arc::from(path),
        placeholder,
    })
}

fn managed_filename(name: &std::ffi::OsStr) -> bool {
    let Some((hash, extension)) = name.to_str().and_then(|name| name.rsplit_once('.')) else {
        return false;
    };
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && matches!(
            extension,
            "png" | "jpg" | "gif" | "webp" | "bmp" | "tiff" | "ico"
        )
}

fn file_uri(path: &Path) -> io::Result<String> {
    let absolute = fs::canonicalize(path)?;
    url::Url::from_file_path(absolute)
        .map(String::from)
        .map_err(|_| io::Error::other("Artwork path cannot be represented as a file URI"))
}

fn write_image(path: &Path, data: &[u8]) -> io::Result<String> {
    if !path.is_file() {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Artwork has no directory"))?;
        fs::create_dir_all(parent)?;
        // Rename a complete file into place. The cache is disposable: no fsync.
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(data)?;
        temporary.persist(path).map_err(|error| error.error)?;
    }
    file_uri(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs::File;

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
        assert!(manager.cached(&selected).unwrap().is_placeholder());
        let first = manager.load(&selected).unwrap();
        // Matching the bundled bytes does not turn an embedded cover into a fallback.
        assert!(!first.is_placeholder());
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
        assert_eq!(
            result_uri(manager.load(&selected)),
            result_uri(manager.default.clone())
        );
        assert!(manager.cached(&selected).unwrap().is_placeholder());
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
            let failed = manager.load(&selected).unwrap();
            assert!(failed.is_placeholder());
            assert!(manager.is_loaded(&selected));
            assert_eq!(
                manager.load_with(&selected, || panic!("unchanged failure reprobed")),
                Some(failed)
            );

            selected.path = cover_source.clone();
            assert!(!manager.is_loaded(&selected));
            assert!(manager.cached(&selected).unwrap().is_placeholder());
            let recovered = manager.load(&selected).unwrap();
            assert!(!recovered.is_placeholder());
            assert_eq!(fs::read(file_path(recovered.uri())).unwrap(), DEFAULT_IMAGE);
            selected.path = directory.path().join("moved-again.wav");
            assert_eq!(
                manager.load_with(&selected, || panic!("successful content reprobed")),
                Some(recovered)
            );
        }
    }

    #[test]
    fn successful_no_cover_result_is_reused_after_path_changes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("no-cover.wav");
        tagged_wav_image(&source, b"unsupported image bytes");
        let manager = ArtworkManager::new(directory.path());
        let mut selected = track(source);
        let no_cover = manager.load(&selected).unwrap();
        assert!(no_cover.is_placeholder());
        selected.path = directory.path().join("moved.wav");
        assert!(manager.is_loaded(&selected));
        assert_eq!(
            manager.load_with(&selected, || panic!("successful no-cover result reprobed")),
            Some(no_cover)
        );
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
                                manager.embedded_uri(&selected.path)
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
        let fallback = result_uri(manager.default.clone());
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
        assert_eq!(direct, fallback);
        assert_eq!(linked, fallback);
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
                    Ok(None)
                })
            });
            loading.recv().unwrap();
            scope.spawn(move || {
                read.send((manager.cached(selected), manager.is_loaded(selected)))
                    .unwrap()
            });
            let observed = reads.recv_timeout(Duration::from_secs(2));
            release.send(()).unwrap();
            assert_eq!(observed.unwrap(), (manager.default.clone(), false));
            loader.join().unwrap();
        });
        let _cleaning = manager.maintenance.write();
        assert!(manager.is_loaded(&selected));
        assert_eq!(
            result_uri(manager.cached(&selected)),
            result_uri(manager.default.clone())
        );
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
                    manager.embedded_uri(source)
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
        let mut selected = track(directory.path().join("missing"));
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
    fn cleanup_waits_for_first_acquisition_and_preserves_newly_registered_file() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let selected = track(source);
        let cache = directory.path().join("artwork/cache");
        fs::create_dir_all(&cache).unwrap();
        let path = cache.join(format!("{}.png", blake3::hash(DEFAULT_IMAGE)));
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
                    manager.embedded_uri(&selected.path)
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
        assert_eq!(
            result_uri(manager.load(&missing)),
            result_uri(manager.default.clone())
        );
        assert_eq!(
            result_uri(manager.load_with(&missing, || panic!("cached failure reprobed"))),
            result_uri(manager.default.clone())
        );
        assert_eq!(manager.maintain(Duration::ZERO, false).unwrap().files, 0);
        fs::write(directory.path().join("artwork/cache"), b"blocked").unwrap();
        assert!(manager.maintain(Duration::ZERO, false).is_err());
        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let unavailable = ArtworkManager::new(&blocked);
        assert_eq!(unavailable.load(&missing), None);
        assert!(unavailable.is_loaded(&missing));
    }
}
