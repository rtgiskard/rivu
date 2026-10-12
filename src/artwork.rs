//! Local, best-effort artwork shared by the application session. Cache-only
//! reads never probe media; blocking loads belong on existing background workers.
//! No decoder or audio output is opened; failures never propagate into playback.

use crate::model::Track;
use parking_lot::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
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
const MAX_DISK_ENTRIES: usize = 128;
const MAX_DISK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const RETIRED: usize = usize::MAX;

struct ArtworkFile {
    uri: String,
    path: Arc<Path>,
    bytes: u64,
    // Zero means collectible, positive values are consumer leases. Retirement
    // and acquisition use one CAS boundary, without filesystem locks in reads.
    leases: AtomicUsize,
}

impl ArtworkFile {
    fn acquire(self: &Arc<Self>) -> Option<ArtworkLease> {
        self.leases
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count != RETIRED).then(|| count + 1)
            })
            .ok()?;
        Some(ArtworkLease(Arc::clone(self)))
    }
}

/// A published URI/path stays valid until the consumer replaces or drops it.
pub struct ArtworkLease(Arc<ArtworkFile>);

impl ArtworkLease {
    pub fn uri(&self) -> &str {
        &self.0.uri
    }
    pub(crate) fn path(&self) -> Arc<Path> {
        Arc::clone(&self.0.path)
    }
}

impl std::fmt::Debug for ArtworkLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ArtworkLease").field(&self.uri()).finish()
    }
}

impl PartialEq for ArtworkLease {
    fn eq(&self, other: &Self) -> bool {
        self.uri() == other.uri()
    }
}

impl Eq for ArtworkLease {}

impl Clone for ArtworkLease {
    fn clone(&self) -> Self {
        self.0.leases.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(&self.0))
    }
}

impl Drop for ArtworkLease {
    fn drop(&mut self) {
        self.0.leases.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Default)]
struct ArtworkDisk {
    files: HashMap<PathBuf, Arc<ArtworkFile>>,
    order: VecDeque<PathBuf>,
    bytes: u64,
}

impl ArtworkDisk {
    // Only startup and the existing blocking loaders perform collection/I/O.
    // Admission fails rather than exceed a budget whose files are all leased.
    fn make_room(&mut self, bytes: u64, entries: usize) -> bool {
        let mut remaining = self.order.len();
        while self.bytes + bytes > MAX_DISK_BYTES || self.files.len() + entries > MAX_DISK_ENTRIES {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
            let Some(path) = self.order.pop_front() else {
                return false;
            };
            let file = &self.files[&path];
            if file
                .leases
                .compare_exchange(0, RETIRED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        self.bytes -= file.bytes;
                        self.files.remove(&path);
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        self.bytes -= file.bytes;
                        self.files.remove(&path);
                        continue;
                    }
                    Err(_) => {
                        file.leases.store(0, Ordering::Release);
                    }
                }
            }
            self.order.push_back(path);
        }
        true
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
    fingerprint: Option<String>,
    image: OnceLock<Option<Arc<ArtworkFile>>>,
}

#[derive(Default)]
struct ArtworkCache {
    items: HashMap<i64, Arc<CachedArtwork>>,
    insertion_order: VecDeque<i64>,
}

pub struct ArtworkManager {
    directory: PathBuf,
    default: Option<Arc<ArtworkFile>>,
    disk: Mutex<ArtworkDisk>,
    // The lookup key is exactly (track ID, fingerprint). Index by ID and compare
    // the fingerprint in place, avoiding a fingerprint allocation on cache hits.
    entries: Mutex<ArtworkCache>,
}

impl ArtworkManager {
    pub fn new(data_dir: &Path) -> Self {
        let directory = data_dir.join("artwork");
        let default = write_image(&directory.join("default.png"), DEFAULT_IMAGE)
            .ok()
            .map(|uri| artwork_file(uri, DEFAULT_IMAGE.len() as u64));
        let mut disk = ArtworkDisk::default();
        if let Ok(entries) = fs::read_dir(directory.join("cache")) {
            let mut files: Vec<_> = entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let metadata = entry.metadata().ok()?;
                    if !metadata.is_file() {
                        return None;
                    }
                    Some((metadata.modified().ok(), entry.path(), metadata.len()))
                })
                .collect();
            files.sort_by_key(|(modified, _, _)| *modified);
            for (_, path, bytes) in files {
                if let Ok(uri) = file_uri(&path) {
                    disk.bytes += bytes;
                    disk.order.push_back(path.clone());
                    disk.files.insert(path, artwork_file(uri, bytes));
                }
            }
            disk.make_room(0, 0);
        }
        Self {
            directory,
            default,
            disk: Mutex::new(disk),
            entries: Mutex::new(ArtworkCache::default()),
        }
    }

    /// Cache-only lease acquisition: no filesystem work or probe waiting.
    pub fn cached(&self, track: &Track) -> Option<ArtworkLease> {
        self.entries
            .lock()
            .items
            .get(&track.id)
            .filter(|cached| cached.fingerprint == track.fingerprint)
            .and_then(|cached| cached.image.get())
            .and_then(|file| file.as_ref())
            .and_then(ArtworkFile::acquire)
            .or_else(|| self.default.as_ref().and_then(ArtworkFile::acquire))
    }

    pub fn is_loaded(&self, track: &Track) -> bool {
        self.entries
            .lock()
            .items
            .get(&track.id)
            .is_some_and(|cached| {
                cached.fingerprint == track.fingerprint
                    && cached.image.get().is_some_and(|image| {
                        image
                            .as_ref()
                            .is_none_or(|file| file.leases.load(Ordering::Acquire) != RETIRED)
                    })
            })
    }

    /// Blocking, singleflight load for this (ID, fingerprint). The map lock is
    /// released before probing or waiting for another caller's in-flight load.
    pub fn load(&self, track: &Track) -> Option<ArtworkLease> {
        self.load_with(track, || self.embedded_uri(&track.path))
    }

    fn load_with(
        &self,
        track: &Track,
        probe: impl FnOnce() -> Option<ArtworkLease>,
    ) -> Option<ArtworkLease> {
        let cached = {
            let mut cache = self.entries.lock();
            let ArtworkCache {
                items: entries,
                insertion_order: order,
            } = &mut *cache;
            if let Some(cached) = entries.get(&track.id)
                && cached.fingerprint == track.fingerprint
                && cached.image.get().is_none_or(|image| {
                    image
                        .as_ref()
                        .is_none_or(|file| file.leases.load(Ordering::Acquire) != RETIRED)
                })
            {
                Arc::clone(cached)
            } else {
                // Fingerprint replacement keeps its existing FIFO position.
                if !entries.contains_key(&track.id) {
                    if entries.len() == MAX_ARTWORK_ENTRIES
                        && let Some(oldest) = order.pop_front()
                    {
                        entries.remove(&oldest);
                    }
                    order.push_back(track.id);
                }
                let cached = Arc::new(CachedArtwork {
                    fingerprint: track.fingerprint.clone(),
                    image: OnceLock::new(),
                });
                entries.insert(track.id, Arc::clone(&cached));
                cached
            }
        };
        // Same-key requests share one probe without holding the map lock. An
        // older fingerprint may finish later but cannot replace its successor.
        // Eviction drops only the manager's Arc; in-flight callers stay valid.
        // Revisiting an evicted key starts a new probe, never reinserts old work.
        // Hold the initializer's lease until this caller has acquired its own:
        // another background load cannot retire a just-completed image.
        let mut initializing = None;
        let image = cached.image.get_or_init(|| {
            initializing = probe().or_else(|| self.default.as_ref().and_then(ArtworkFile::acquire));
            initializing.as_ref().map(|lease| Arc::clone(&lease.0))
        });
        image
            .as_ref()
            .and_then(ArtworkFile::acquire)
            .or_else(|| self.default.as_ref().and_then(ArtworkFile::acquire))
    }

    fn embedded_uri(&self, path: &Path) -> Option<ArtworkLease> {
        let file = File::open(path).ok()?;
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
            .ok()?;
        let track_id = format
            .default_track(TrackType::Audio)
            .map(|track| u64::from(track.id));
        self.metadata_uri(format.metadata(), track_id)
    }

    fn metadata_uri(
        &self,
        mut metadata: Metadata<'_>,
        track_id: Option<u64>,
    ) -> Option<ArtworkLease> {
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

    fn visual_uri(&self, visual: &Visual) -> Option<ArtworkLease> {
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
        let mut disk = self.disk.lock();
        if let Some(file) = disk.files.get(&path) {
            return file.acquire();
        }
        if !disk.make_room(visual.data.len() as u64, 1) {
            return None;
        }
        let uri = write_image(&path, &visual.data).ok()?;
        let file = artwork_file(uri, visual.data.len() as u64);
        let lease = file.acquire()?;
        disk.bytes += file.bytes;
        disk.order.push_back(path.clone());
        disk.files.insert(path, file);
        Some(lease)
    }
}

fn artwork_file(uri: String, bytes: u64) -> Arc<ArtworkFile> {
    let path = url::Url::parse(&uri)
        .expect("artwork file URI")
        .to_file_path()
        .expect("local artwork");
    Arc::new(ArtworkFile {
        uri,
        path: Arc::from(path),
        bytes,
        leases: AtomicUsize::new(0),
    })
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
mod tests {
    use super::*;

    fn track(path: PathBuf) -> Track {
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
            track_number: None,
            disc_number: None,
            bits_per_sample: Some(16),
            release_date: None,
            favorite: false,
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    fn tagged_wav(path: &Path) {
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

    fn fallback(manager: &ArtworkManager) -> Option<ArtworkLease> {
        manager.default.as_ref().and_then(ArtworkFile::acquire)
    }

    fn artificial(uri: &str) -> ArtworkLease {
        artwork_file(uri.into(), 0).acquire().unwrap()
    }

    #[test]
    fn embedded_picture_is_content_addressed_and_key_uses_only_id_and_fingerprint() {
        let directory = tempfile::Builder::new()
            .prefix("rivu artwork #")
            .tempdir()
            .unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let mut track = track(source);
        let uri = manager.load(&track).unwrap();
        let expected = directory
            .path()
            .join("artwork/cache")
            .join(format!("{}.png", blake3::hash(DEFAULT_IMAGE)));
        assert_eq!(file_path(uri.uri()), expected);
        assert_eq!(fs::read(&expected).unwrap(), DEFAULT_IMAGE);
        assert!(uri.uri().contains("%23"));

        // Another track with the same picture reuses the content-addressed file.
        track.id = 2;
        track.fingerprint = Some("other".into());
        assert_eq!(manager.load(&track), Some(uri.clone()));
        assert_eq!(fs::read_dir(expected.parent().unwrap()).unwrap().count(), 1);

        // Path and editable metadata are deliberately not cache-key components.
        track.path = directory.path().join("missing.wav");
        track.title = "Edited title".into();
        assert_eq!(manager.load(&track), Some(uri.clone()));
        track.fingerprint = Some("changed".into());
        assert_eq!(manager.load(&track), fallback(&manager));
        track.id = 3;
        track.fingerprint = Some("original".into());
        assert_eq!(manager.load(&track), fallback(&manager));
    }

    #[test]
    fn concurrent_requests_probe_once_and_share_a_complete_image() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let track = track(source);
        let barrier = std::sync::Barrier::new(4);
        let probes = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        manager
                            .load_with(&track, || {
                                probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                manager.embedded_uri(&track.path)
                            })
                            .unwrap()
                    })
                })
                .collect();
            for handle in handles {
                let uri = handle.join().unwrap();
                assert_eq!(fs::read(file_path(uri.uri())).unwrap(), DEFAULT_IMAGE);
                assert_eq!(manager.load(&track), Some(uri));
            }
        });
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn cached_reads_return_fallback_without_waiting_for_a_cold_load() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let track = track(directory.path().join("missing.wav"));
        assert_eq!(manager.cached(&track), fallback(&manager));
        assert!(!manager.is_loaded(&track));
        assert!(manager.entries.lock().items.is_empty());
        let (started, loading) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (read, reads) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let manager = &manager;
            let track = &track;
            let loader = scope.spawn(move || {
                manager.load_with(track, || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    Some(artificial("file:///cover.png"))
                })
            });
            loading.recv().unwrap();
            scope.spawn(move || {
                read.send((manager.cached(track), manager.is_loaded(track)))
                    .unwrap()
            });
            let during_load = reads.recv_timeout(std::time::Duration::from_secs(2));
            release.send(()).unwrap();
            assert_eq!(during_load.unwrap(), (fallback(manager), false));
            assert_eq!(loader.join().unwrap().unwrap().uri(), "file:///cover.png");
        });
        assert!(manager.is_loaded(&track));
        assert_eq!(
            manager.cached(&track).as_ref().map(ArtworkLease::uri),
            Some("file:///cover.png")
        );
    }

    #[test]
    fn old_fingerprint_cannot_overwrite_new_cache_entry() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let mut track = track(directory.path().join("missing.wav"));
        let old = Arc::new(CachedArtwork {
            fingerprint: track.fingerprint.clone(),
            image: OnceLock::new(),
        });
        manager.entries.lock().items.insert(track.id, old.clone());
        manager.entries.lock().insertion_order.push_back(track.id);
        track.fingerprint = Some("new".into());
        let current_uri = manager.load(&track);
        assert!(
            old.image
                .set(Some(artwork_file("file:///old.png".into(), 0)))
                .is_ok()
        );
        assert_eq!(manager.cached(&track), current_uri);
        let entries = manager.entries.lock();
        let entries = &entries.items;
        assert!(!Arc::ptr_eq(entries.get(&track.id).unwrap(), &old));
        assert_eq!(
            entries.get(&track.id).unwrap().fingerprint,
            track.fingerprint
        );
    }

    #[test]
    fn eviction_bounds_retention_and_keeps_outstanding_entries_valid() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let mut track = track(directory.path().join("missing.wav"));
        let first_id = track.id;
        let pending = Arc::new(CachedArtwork {
            fingerprint: track.fingerprint.clone(),
            image: OnceLock::new(),
        });
        manager
            .entries
            .lock()
            .items
            .insert(first_id, Arc::clone(&pending));
        manager.entries.lock().insertion_order.push_back(first_id);

        for offset in 1..=MAX_ARTWORK_ENTRIES * 2 {
            track.id = first_id + offset as i64;
            manager.load(&track);
            let cache = manager.entries.lock();
            assert!(cache.items.len() <= MAX_ARTWORK_ENTRIES);
            assert!(cache.insertion_order.len() <= MAX_ARTWORK_ENTRIES);
        }
        assert!(!manager.entries.lock().items.contains_key(&first_id));
        assert!(
            pending
                .image
                .set(Some(artwork_file("file:///old.png".into(), 0)))
                .is_ok()
        );
        assert_eq!(
            pending.image.get().unwrap().as_ref().unwrap().uri,
            "file:///old.png"
        );

        track.id = first_id;
        track.fingerprint = Some("replacement".into());
        assert_eq!(manager.load(&track), fallback(&manager));
        let entries = manager.entries.lock();
        let entries = &entries.items;
        assert_eq!(entries.len(), MAX_ARTWORK_ENTRIES);
        let replacement = entries.get(&first_id).unwrap();
        assert!(!Arc::ptr_eq(replacement, &pending));
        assert_eq!(replacement.fingerprint, track.fingerprint);
    }

    #[test]
    fn failures_fall_back_to_default_or_omit_the_uri() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let default = &manager.default.as_ref().unwrap().uri;
        assert_eq!(fs::read(file_path(default)).unwrap(), DEFAULT_IMAGE);
        fs::write(directory.path().join("artwork/cache"), b"not a directory").unwrap();
        assert_eq!(manager.load(&track(source)), fallback(&manager));

        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let unavailable = ArtworkManager::new(&blocked);
        let missing = track(directory.path().join("missing"));
        assert!(!unavailable.is_loaded(&missing));
        assert_eq!(unavailable.cached(&missing), None);
        assert_eq!(unavailable.load(&missing), None);
        assert!(unavailable.is_loaded(&missing));
        assert_eq!(
            unavailable.load_with(&missing, || panic!("failed load must be cached")),
            None
        );
    }

    fn disk_test_directory() -> tempfile::TempDir {
        let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
        fs::create_dir_all(&cache).unwrap();
        tempfile::tempdir_in(cache).unwrap()
    }

    #[test]
    fn different_content_is_bounded_across_sessions_and_active_leases_survive_pressure() {
        let directory = disk_test_directory();
        let source = directory.path().join("track.wav");
        let mut selected = track(source.clone());
        for session in 0..3 {
            let manager = ArtworkManager::new(directory.path());
            let mut image = DEFAULT_IMAGE.to_vec();
            image.extend_from_slice(&(session as u32).to_le_bytes());
            tagged_wav_image(&source, &image);
            selected.fingerprint = Some(format!("{session}-active"));
            let active = manager.load(&selected).unwrap();
            let active_path = file_path(active.uri());
            for content in 0..MAX_DISK_ENTRIES * 2 {
                let mut image = DEFAULT_IMAGE.to_vec();
                image.extend_from_slice(&((session * 1000 + content + 10) as u32).to_le_bytes());
                tagged_wav_image(&source, &image);
                selected.fingerprint = Some(format!("{session}-{content}"));
                drop(manager.load(&selected));
                assert!(active_path.is_file());
                let files: Vec<_> = fs::read_dir(directory.path().join("artwork/cache"))
                    .unwrap()
                    .map(|entry| entry.unwrap().metadata().unwrap().len())
                    .collect();
                assert!(files.len() <= MAX_DISK_ENTRIES);
                assert!(files.iter().sum::<u64>() <= MAX_DISK_BYTES);
            }
            assert_eq!(
                fs::read(&active_path).unwrap().len(),
                DEFAULT_IMAGE.len() + 4
            );
            drop(active);
            // Released files can retire even while their lookup record exists.
            manager.disk.lock().make_room(MAX_DISK_BYTES, 0);
            assert!(!active_path.exists());
        }
    }

    #[test]
    fn byte_budget_refuses_admission_while_leased_and_recovers_after_release() {
        let directory = disk_test_directory();
        let cache = directory.path().join("artwork/cache");
        fs::create_dir_all(&cache).unwrap();
        for index in 0..MAX_DISK_BYTES / MAX_IMAGE_BYTES as u64 {
            let file = File::create(cache.join(format!("{index}.png"))).unwrap();
            file.set_len(MAX_IMAGE_BYTES as u64).unwrap();
        }
        let manager = ArtworkManager::new(directory.path());
        let mut leases: Vec<_> = manager
            .disk
            .lock()
            .files
            .values()
            .map(|file| file.acquire().unwrap())
            .collect();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let mut selected = track(source);
        assert_eq!(manager.load(&selected), fallback(&manager));
        assert_eq!(manager.disk.lock().bytes, MAX_DISK_BYTES);
        drop(leases.pop());
        selected.fingerprint = Some("retry".into());
        let cover = manager.load(&selected).unwrap();
        assert_ne!(Some(cover.clone()), fallback(&manager));
        assert_eq!(fs::read(file_path(cover.uri())).unwrap(), DEFAULT_IMAGE);
        assert!(manager.disk.lock().bytes <= MAX_DISK_BYTES);
        drop(leases);
        drop(cover);
        drop(manager);
        let recovered = ArtworkManager::new(directory.path());
        assert!(recovered.disk.lock().bytes <= MAX_DISK_BYTES);
    }

    #[test]
    fn new_session_collects_legacy_excess_by_bytes_and_entries() {
        let directory = disk_test_directory();
        let cache = directory.path().join("artwork/cache");
        fs::create_dir_all(&cache).unwrap();
        for index in 0..MAX_DISK_ENTRIES + 3 {
            let file = File::create(cache.join(format!("{index}.png"))).unwrap();
            file.set_len(MAX_DISK_BYTES / MAX_DISK_ENTRIES as u64 + 1)
                .unwrap();
        }
        let manager = ArtworkManager::new(directory.path());
        let disk = manager.disk.lock();
        assert!(disk.files.len() <= MAX_DISK_ENTRIES);
        assert!(disk.bytes <= MAX_DISK_BYTES);
        assert_eq!(fs::read_dir(cache).unwrap().count(), disk.files.len());
    }

    #[test]
    fn released_disk_entry_invalidates_resident_lookup_and_can_reload() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let selected = track(source);
        let lease = manager.load(&selected).unwrap();
        let path = file_path(lease.uri());
        assert!(manager.is_loaded(&selected));
        drop(lease);
        assert!(manager.disk.lock().make_room(MAX_DISK_BYTES, 0));
        assert!(!path.exists());
        assert!(!manager.is_loaded(&selected));
        assert_eq!(manager.cached(&selected), fallback(&manager));
        let reloaded = manager.load(&selected).unwrap();
        assert_eq!(file_path(reloaded.uri()), path);
        assert_eq!(fs::read(path).unwrap(), DEFAULT_IMAGE);
    }
}
