use super::ScanCancellation;
use crate::execution::BlockingPool;
use anyhow::{Context, Result, bail};
use rivu_audio::{MediaInfo, SourceIdentity, probe_file};
use rivu_format::input::open_regular_file;
use rivu_protocol::model::CueSegment;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek};
use std::path::PathBuf;
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
    pub cue: Option<CueSegment>,
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
    workers: &BlockingPool,
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
            let (remaining, completed, buffer) = workers
                .execute(move || {
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
