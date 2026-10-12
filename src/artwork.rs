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
    sync::{Arc, OnceLock},
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
    uri: OnceLock<Option<String>>,
    path: OnceLock<Option<Arc<Path>>>,
}

#[derive(Default)]
struct ArtworkCache {
    items: HashMap<i64, Arc<CachedArtwork>>,
    insertion_order: VecDeque<i64>,
}

pub struct ArtworkManager {
    directory: PathBuf,
    default_uri: Option<String>,
    default_path: Option<Arc<Path>>,
    // The lookup key is exactly (track ID, fingerprint). Index by ID and compare
    // the fingerprint in place, avoiding a fingerprint allocation on cache hits.
    entries: Mutex<ArtworkCache>,
}

impl ArtworkManager {
    pub fn new(data_dir: &Path) -> Self {
        let directory = data_dir.join("artwork");
        let default_uri = write_image(&directory.join("default.png"), DEFAULT_IMAGE).ok();
        let default_path = default_uri
            .as_ref()
            .map(|_| Arc::from(directory.join("default.png")));
        Self {
            directory,
            default_uri,
            default_path,
            entries: Mutex::new(ArtworkCache::default()),
        }
    }

    /// Returns a completed URI or the session fallback, without media/file I/O
    /// or waiting for an in-flight probe. Failed loads are completed entries too.
    pub fn cached_uri(&self, track: &Track) -> Option<String> {
        let cache = self.entries.lock();
        cache
            .items
            .get(&track.id)
            .filter(|cached| cached.fingerprint == track.fingerprint)
            .and_then(|cached| cached.uri.get())
            .cloned()
            .unwrap_or_else(|| self.default_uri.clone())
    }

    /// GPUI local resources use paths, not HTTP-backed URI resources.
    pub(crate) fn cached_path(&self, track: &Track) -> Option<Arc<Path>> {
        self.entries
            .lock()
            .items
            .get(&track.id)
            .filter(|cached| cached.fingerprint == track.fingerprint)
            .and_then(|cached| cached.path.get())
            .cloned()
            .unwrap_or_else(|| self.default_path.clone())
    }

    pub fn is_loaded(&self, track: &Track) -> bool {
        self.entries
            .lock()
            .items
            .get(&track.id)
            .is_some_and(|cached| {
                cached.fingerprint == track.fingerprint && cached.uri.get().is_some()
            })
    }

    /// Blocking, singleflight load for this (ID, fingerprint). The map lock is
    /// released before probing or waiting for another caller's in-flight load.
    pub fn load(&self, track: &Track) -> Option<String> {
        self.load_with(track, || self.embedded_uri(&track.path))
    }

    fn load_with(&self, track: &Track, probe: impl FnOnce() -> Option<String>) -> Option<String> {
        let cached = {
            let mut cache = self.entries.lock();
            let ArtworkCache {
                items: entries,
                insertion_order: order,
            } = &mut *cache;
            if let Some(cached) = entries.get(&track.id)
                && cached.fingerprint == track.fingerprint
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
                    uri: OnceLock::new(),
                    path: OnceLock::new(),
                });
                entries.insert(track.id, Arc::clone(&cached));
                cached
            }
        };
        // Same-key requests share one probe without holding the map lock. An
        // older fingerprint may finish later but cannot replace its successor.
        // Eviction drops only the manager's Arc; in-flight callers stay valid.
        // Revisiting an evicted key starts a new probe, never reinserts old work.
        cached
            .uri
            .get_or_init(|| {
                let uri = probe().or_else(|| self.default_uri.clone());
                let path = uri
                    .as_ref()
                    .and_then(|uri| url::Url::parse(uri).ok())
                    .and_then(|url| url.to_file_path().ok())
                    .map(Arc::from);
                let _ = cached.path.set(path);
                uri
            })
            .clone()
    }

    fn embedded_uri(&self, path: &Path) -> Option<String> {
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

    fn metadata_uri(&self, mut metadata: Metadata<'_>, track_id: Option<u64>) -> Option<String> {
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

    fn visual_uri(&self, visual: &Visual) -> Option<String> {
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
        write_image(&path, &visual.data).ok()
    }
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
    let absolute = fs::canonicalize(path)?;
    url::Url::from_file_path(absolute)
        .map(String::from)
        .map_err(|_| io::Error::other("Artwork path cannot be represented as a file URI"))
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
        // ID3v2.3 APIC followed by one mono PCM WAV sample. Probing metadata
        // requires neither a decoder nor an audio device.
        let mut picture = b"\0image/png\0\x03\0".to_vec();
        picture.extend_from_slice(DEFAULT_IMAGE);
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
        assert_eq!(file_path(&uri), expected);
        assert_eq!(fs::read(&expected).unwrap(), DEFAULT_IMAGE);
        assert!(uri.contains("%23"));

        // Another track with the same picture reuses the content-addressed file.
        track.id = 2;
        track.fingerprint = Some("other".into());
        assert_eq!(manager.load(&track).as_deref(), Some(uri.as_str()));
        assert_eq!(fs::read_dir(expected.parent().unwrap()).unwrap().count(), 1);

        // Path and editable metadata are deliberately not cache-key components.
        track.path = directory.path().join("missing.wav");
        track.title = "Edited title".into();
        assert_eq!(manager.load(&track).as_deref(), Some(uri.as_str()));
        track.fingerprint = Some("changed".into());
        assert_eq!(manager.load(&track), manager.default_uri);
        track.id = 3;
        track.fingerprint = Some("original".into());
        assert_eq!(manager.load(&track), manager.default_uri);
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
                assert_eq!(fs::read(file_path(&uri)).unwrap(), DEFAULT_IMAGE);
                assert_eq!(manager.load(&track).as_deref(), Some(uri.as_str()));
            }
        });
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn cached_reads_return_fallback_without_waiting_for_a_cold_load() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let track = track(directory.path().join("missing.wav"));
        assert_eq!(manager.cached_uri(&track), manager.default_uri);
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
                    Some("file:///cover.png".into())
                })
            });
            loading.recv().unwrap();
            scope.spawn(move || {
                read.send((manager.cached_uri(track), manager.is_loaded(track)))
                    .unwrap()
            });
            let during_load = reads.recv_timeout(std::time::Duration::from_secs(2));
            release.send(()).unwrap();
            assert_eq!(during_load.unwrap(), (manager.default_uri.clone(), false));
            assert_eq!(loader.join().unwrap().as_deref(), Some("file:///cover.png"));
        });
        assert!(manager.is_loaded(&track));
        assert_eq!(
            manager.cached_uri(&track).as_deref(),
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
            uri: OnceLock::new(),
            path: OnceLock::new(),
        });
        manager.entries.lock().items.insert(track.id, old.clone());
        manager.entries.lock().insertion_order.push_back(track.id);
        track.fingerprint = Some("new".into());
        let current_uri = manager.load(&track);
        old.uri.set(Some("file:///old.png".into())).unwrap();
        assert_eq!(manager.cached_uri(&track), current_uri);
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
            uri: OnceLock::new(),
            path: OnceLock::new(),
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
        pending.uri.set(Some("file:///old.png".into())).unwrap();
        assert_eq!(
            pending.uri.get().unwrap().as_deref(),
            Some("file:///old.png")
        );

        track.id = first_id;
        track.fingerprint = Some("replacement".into());
        assert_eq!(manager.load(&track), manager.default_uri);
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
        let default = manager.default_uri.as_deref().unwrap();
        assert_eq!(fs::read(file_path(default)).unwrap(), DEFAULT_IMAGE);
        fs::write(directory.path().join("artwork/cache"), b"not a directory").unwrap();
        assert_eq!(manager.load(&track(source)), manager.default_uri);

        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let unavailable = ArtworkManager::new(&blocked);
        let missing = track(directory.path().join("missing"));
        assert!(!unavailable.is_loaded(&missing));
        assert_eq!(unavailable.cached_uri(&missing), None);
        assert_eq!(unavailable.load(&missing), None);
        assert!(unavailable.is_loaded(&missing));
        assert_eq!(
            unavailable.load_with(&missing, || panic!("failed load must be cached")),
            None
        );
    }
}
