//! Local, best-effort artwork for desktop media controls. No decoder or audio
//! output is opened, and cache failures never propagate into playback.

use crate::model::Track;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
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

struct CachedArtwork {
    fingerprint: Option<String>,
    uri: OnceLock<Option<String>>,
}

pub struct ArtworkManager {
    directory: PathBuf,
    default_uri: Option<String>,
    // The lookup key is exactly (track ID, fingerprint). Index by ID and compare
    // the fingerprint in place, avoiding a fingerprint allocation on cache hits.
    entries: Mutex<HashMap<i64, Arc<CachedArtwork>>>,
}

impl ArtworkManager {
    pub fn new(data_dir: &Path) -> Self {
        let directory = data_dir.join("artwork");
        let default_uri = write_image(&directory.join("default.png"), DEFAULT_IMAGE).ok();
        Self {
            directory,
            default_uri,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn uri_for(&self, track: &Track) -> Option<String> {
        let cached = {
            let mut entries = self.entries.lock();
            let cached = entries.entry(track.id).or_insert_with(|| {
                Arc::new(CachedArtwork {
                    fingerprint: track.fingerprint.clone(),
                    uri: OnceLock::new(),
                })
            });
            if cached.fingerprint != track.fingerprint {
                *cached = Arc::new(CachedArtwork {
                    fingerprint: track.fingerprint.clone(),
                    uri: OnceLock::new(),
                });
            }
            Arc::clone(cached)
        };
        // Same-key requests share one probe without holding the map lock. An
        // older fingerprint may finish later but cannot replace its successor.
        cached
            .uri
            .get_or_init(|| {
                self.embedded_uri(&track.path)
                    .or_else(|| self.default_uri.clone())
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
        let uri = manager.uri_for(&track).unwrap();
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
        assert_eq!(manager.uri_for(&track).as_deref(), Some(uri.as_str()));
        assert_eq!(fs::read_dir(expected.parent().unwrap()).unwrap().count(), 1);

        // Path and editable metadata are deliberately not cache-key components.
        track.path = directory.path().join("missing.wav");
        track.title = "Edited title".into();
        assert_eq!(manager.uri_for(&track).as_deref(), Some(uri.as_str()));
        track.fingerprint = Some("changed".into());
        assert_eq!(manager.uri_for(&track), manager.default_uri);
        track.id = 3;
        track.fingerprint = Some("original".into());
        assert_eq!(manager.uri_for(&track), manager.default_uri);
    }

    #[test]
    fn concurrent_requests_share_a_complete_cached_image() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("track.wav");
        tagged_wav(&source);
        let manager = ArtworkManager::new(directory.path());
        let track = track(source);
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        manager.uri_for(&track).unwrap()
                    })
                })
                .collect();
            for handle in handles {
                let uri = handle.join().unwrap();
                assert_eq!(fs::read(file_path(&uri)).unwrap(), DEFAULT_IMAGE);
                assert_eq!(manager.uri_for(&track).as_deref(), Some(uri.as_str()));
            }
        });
    }

    #[test]
    fn old_fingerprint_cannot_overwrite_new_cache_entry() {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let mut track = track(directory.path().join("missing.wav"));
        let old = Arc::new(CachedArtwork {
            fingerprint: track.fingerprint.clone(),
            uri: OnceLock::new(),
        });
        manager.entries.lock().insert(track.id, old.clone());
        track.fingerprint = Some("new".into());
        let current_uri = manager.uri_for(&track);
        old.uri.set(Some("file:///old.png".into())).unwrap();
        assert_eq!(manager.uri_for(&track), current_uri);
        let entries = manager.entries.lock();
        assert!(!Arc::ptr_eq(entries.get(&track.id).unwrap(), &old));
        assert_eq!(
            entries.get(&track.id).unwrap().fingerprint,
            track.fingerprint
        );
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
        assert_eq!(manager.uri_for(&track(source)), manager.default_uri);

        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let unavailable = ArtworkManager::new(&blocked);
        assert_eq!(
            unavailable.uri_for(&track(directory.path().join("missing"))),
            None
        );
    }
}
