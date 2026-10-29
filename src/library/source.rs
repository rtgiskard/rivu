use super::ScanCancellation;
use crate::execution::BlockingPool;
use anyhow::{Context, Result, bail};
use rivu_audio::{MediaInfo, SourceIdentity, probe_file};
use rivu_format::regular_file::open_regular_file;
use rivu_protocol::model::CueSegment;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
}

/// A subtrack borrows source facts and keeps metadata in its original owner:
/// either one shared parsed sheet or the registered identity's stored labels.
#[derive(Clone, Debug)]
pub struct CueRecord {
    pub source: Arc<ScanRecord>,
    origin: CueOrigin,
    pub duration: Option<f64>,
}

#[derive(Clone, Debug)]
enum CueOrigin {
    Parsed {
        sheet: Arc<super::cue::CueSheet>,
        path: Arc<PathBuf>,
        track: usize,
    },
    // Other checks pay one allocation per registered subtrack so ordinary scans
    // do not carry the stored-label payload inline for every parsed segment.
    Registered(Box<RegisteredCue>),
}

#[derive(Clone, Debug)]
struct RegisteredCue {
    cue: CueSegment,
    metadata: RegisteredCueMetadata,
}

#[derive(Clone, Debug)]
pub struct RegisteredCueMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub release_date: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct CueView<'a> {
    pub sheet: &'a PathBuf,
    pub number: u32,
    pub start_frame: u64,
    pub end_frame: Option<u64>,
}

impl<'a> From<&'a CueSegment> for CueView<'a> {
    fn from(cue: &'a CueSegment) -> Self {
        Self {
            sheet: &cue.sheet,
            number: cue.number,
            start_frame: cue.start_frame,
            end_frame: cue.end_frame,
        }
    }
}

#[derive(Debug)]
pub struct ScanTrack<'a> {
    pub source: &'a ScanRecord,
    pub media: MediaView<'a>,
    pub cue: Option<CueView<'a>>,
    /// A successful CUE establishes this segment or its referenced whole source.
    pub duration_filter_exempt: bool,
}

#[derive(Debug)]
pub struct MediaView<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub duration: Option<f64>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub release_date: Option<&'a str>,
    pub source: &'a MediaInfo,
}

impl ScanRecord {
    pub fn view(&self) -> ScanTrack<'_> {
        ScanTrack {
            source: self,
            media: MediaView {
                title: &self.media.title,
                artist: &self.media.artist,
                album: &self.media.album,
                duration: self.media.duration,
                track_number: self.media.track_number,
                disc_number: self.media.disc_number,
                release_date: self.media.release_date.as_deref(),
                source: &self.media,
            },
            cue: None,
            duration_filter_exempt: false,
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.path.as_os_str().len()
            + self.fingerprint.as_ref().map_or(0, String::len)
            + media_retained_bytes(&self.media)
    }
}

fn media_retained_bytes(media: &MediaInfo) -> usize {
    media.title.capacity()
        + media.artist.capacity()
        + media.album.capacity()
        + media.codec.capacity()
        + media.release_date.as_ref().map_or(0, String::capacity)
}

impl CueRecord {
    pub(super) fn parsed(
        source: Arc<ScanRecord>,
        sheet: Arc<super::cue::CueSheet>,
        path: Arc<PathBuf>,
        track: usize,
        duration: Option<f64>,
    ) -> Self {
        Self {
            source,
            origin: CueOrigin::Parsed { sheet, path, track },
            duration,
        }
    }

    pub fn registered(
        source: Arc<ScanRecord>,
        cue: CueSegment,
        metadata: RegisteredCueMetadata,
        duration: Option<f64>,
    ) -> Self {
        Self {
            source,
            origin: CueOrigin::Registered(Box::new(RegisteredCue { cue, metadata })),
            duration,
        }
    }

    pub fn cue(&self) -> CueView<'_> {
        match &self.origin {
            CueOrigin::Parsed { sheet, path, track } => {
                let track = &sheet.tracks[*track];
                CueView {
                    sheet: path,
                    number: track.number,
                    start_frame: track.start_frame,
                    end_frame: track.end_frame,
                }
            }
            CueOrigin::Registered(registered) => (&registered.cue).into(),
        }
    }

    pub fn view(&self) -> ScanTrack<'_> {
        let mut view = self.source.view();
        match &self.origin {
            CueOrigin::Parsed { sheet, track, .. } => {
                let track = &sheet.tracks[*track];
                view.media.title = track
                    .title
                    .as_deref()
                    .expect("CUE title resolved during preparation");
                view.media.artist = track
                    .performer
                    .as_deref()
                    .or(sheet.performer.as_deref())
                    .unwrap_or(&self.source.media.artist);
                view.media.album = sheet.title.as_deref().unwrap_or(&self.source.media.album);
                view.media.track_number = Some(track.number);
            }
            CueOrigin::Registered(registered) => {
                let metadata = &registered.metadata;
                view.media.title = &metadata.title;
                view.media.artist = &metadata.artist;
                view.media.album = &metadata.album;
                view.media.track_number = metadata.track_number;
                view.media.disc_number = metadata.disc_number;
                view.media.release_date = metadata.release_date.as_deref();
            }
        }
        view.media.duration = self.duration;
        view.cue = Some(self.cue());
        view.duration_filter_exempt = true;
        view
    }
}

pub(crate) const SOURCE_BATCH_SIZE: usize = 32;
pub(super) const HASH_CHUNK_BYTES: usize = 128 * 1024;
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
        budget: Option<&super::input::RetainedBudget>,
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
                let media = known.media.take().filter(|media| {
                    known.cue.is_none()
                        && (!ffmpeg_enabled || (media.channels != 0 && media.sample_rate != 0))
                });
                if let (Some(budget), Some(media)) = (budget, &media) {
                    budget.reserve(media_retained_bytes(media))?;
                }
                self.media = media;
            }
            if self.media.is_none() {
                let file = self.file.try_clone()?;
                let media = probe_file(&self.path, file, ffmpeg_enabled).context("probe audio")?;
                if let Some(budget) = budget {
                    budget.reserve(media_retained_bytes(&media))?;
                }
                self.media = Some(media);
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
        if let Some(budget) = budget {
            budget.reserve(
                std::mem::size_of::<ScanRecord>()
                    + 2 * std::mem::size_of::<usize>()
                    + self.path.as_os_str().len()
                    + 64,
            )?;
        }
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
        }))
    }
}

pub(super) async fn prepare_sources<Fut>(
    workers: &BlockingPool,
    paths: Vec<PathBuf>,
    known_sources: &mut impl FnMut(Vec<PathBuf>) -> Fut,
    sources: &mut HashMap<PathBuf, std::result::Result<Arc<ScanRecord>, String>>,
    ffmpeg_enabled: bool,
    (cancellation, budget): (&ScanCancellation, Option<&super::input::RetainedBudget>),
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
        let admitted = workers
            .execute(move || {
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
                    if budget.is_some()
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidInput)
                    {
                        return Err(error.context("Import source is not a regular file"));
                    }
                    let error = format!("{error:#}");
                    sources.insert(path, Err(error));
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
            let retained_budget = budget.cloned();
            let (remaining, completed, buffer) = workers
                .execute(move || {
                    let mut completed = Vec::new();
                    for _ in 0..SOURCE_STEPS_PER_TURN {
                        token.check()?;
                        let Some(source) = pending.front_mut() else {
                            break;
                        };
                        match source.step(
                            ffmpeg_enabled,
                            &token,
                            &mut buffer,
                            retained_budget.as_ref(),
                        ) {
                            Ok(Some(record)) => {
                                pending.pop_front();
                                completed.push((record.path.clone(), Ok(record)));
                            }
                            Ok(None) => {}
                            Err(error) => {
                                token.check()?;
                                if super::input::RetainedBudget::is_limit(&error) {
                                    return Err(error);
                                }
                                let source = pending.pop_front().expect("front source is present");
                                if let (Some(budget), Some(media)) =
                                    (retained_budget.as_ref(), &source.media)
                                {
                                    budget.release(media_retained_bytes(media));
                                }
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
            for (path, record) in completed {
                sources.insert(path, record.map(Arc::new));
            }
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
