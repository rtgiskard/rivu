use super::paths::{is_audio_path, logical_path};
use super::scan::MAX_REPORTED_SCAN_ERRORS;
use super::source::{HASH_CHUNK_BYTES, SOURCE_BATCH_SIZE, ScanRecord, prepare_sources};
use super::{KnownFile, ScanCancellation};
use crate::execution::BlockingPool;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

/// Prepared ordinary sources and their requested queue occurrences. No database
/// writes, traversal, playlist/CUE expansion or duration filtering occur here.
#[derive(Debug, Default)]
pub(crate) struct ExternalAdmission {
    pub(crate) records: Vec<ScanRecord>,
    pub(crate) occurrences: Vec<usize>,
    pub(crate) rejected: usize,
    pub(crate) errors: Vec<String>,
}

impl ExternalAdmission {
    fn report(&mut self, error: String) {
        if self.errors.len() < MAX_REPORTED_SCAN_ERRORS - 1 {
            self.errors.push(error);
        } else if self.errors.len() == MAX_REPORTED_SCAN_ERRORS - 1 {
            self.errors
                .push("Additional external file errors omitted".into());
        }
    }
}

pub(crate) async fn prepare_external_admission<Fut>(
    workers: &BlockingPool,
    paths: Vec<PathBuf>,
    mut known_sources: impl FnMut(Vec<PathBuf>) -> Fut,
    ffmpeg_enabled: bool,
    cancellation: &ScanCancellation,
    mut progress: impl FnMut(rivu_protocol::model::ScanProgress),
) -> Result<ExternalAdmission>
where
    Fut: Future<Output = Result<Vec<Option<KnownFile>>>>,
{
    let requested = paths.len();
    let mut result = ExternalAdmission::default();
    let mut paths = paths.into_iter();
    let mut unique = Vec::new();
    let mut identities = HashMap::new();
    let mut order = Vec::with_capacity(requested);
    let mut normalized_count = 0;
    loop {
        cancellation.check()?;
        let batch: Vec<_> = paths.by_ref().take(SOURCE_BATCH_SIZE).collect();
        if batch.is_empty() {
            break;
        }
        let token = cancellation.clone();
        let normalized = workers
            .execute(move || {
                let mut normalized = Vec::with_capacity(batch.len());
                for path in batch {
                    token.check()?;
                    let candidate = (|| {
                        let path = logical_path(&path)?;
                        path.to_str().context("Audio source path is not UTF-8")?;
                        anyhow::ensure!(is_audio_path(&path), "Not a supported local audio file");
                        Ok::<_, anyhow::Error>(path)
                    })();
                    normalized
                        .push(candidate.map_err(|error| format!("{}: {error:#}", path.display())));
                }
                token.check()?;
                Ok::<_, anyhow::Error>(normalized)
            })
            .await
            .context("External file normalization worker failed")??;
        let normalized_len = normalized.len();
        for candidate in normalized {
            match candidate {
                Ok(path) => {
                    let index = if let Some(&index) = identities.get(&path) {
                        index
                    } else {
                        let index = identities.len();
                        identities.insert(path.clone(), index);
                        unique.push(path);
                        index
                    };
                    order.push(index);
                }
                Err(error) => result.report(error),
            }
        }
        normalized_count += normalized_len;
        progress(rivu_protocol::model::ScanProgress {
            phase: rivu_protocol::model::ScanPhase::Discovering,
            processed: normalized_count,
            total: Some(requested),
            errors: result.errors.len(),
            path: None,
        });
    }
    if unique.is_empty() {
        result.rejected = requested;
        return Ok(result);
    }
    let mut sources = HashMap::new();
    let mut scratch = vec![0; HASH_CHUNK_BYTES];
    let total = unique.len();
    let mut unique = unique.into_iter();
    loop {
        cancellation.check()?;
        let batch: Vec<_> = unique.by_ref().take(SOURCE_BATCH_SIZE).collect();
        if batch.is_empty() {
            break;
        }
        progress(rivu_protocol::model::ScanProgress {
            phase: rivu_protocol::model::ScanPhase::ReadingMetadata,
            processed: sources.len(),
            total: Some(total),
            errors: result.errors.len(),
            path: batch.first().cloned(),
        });
        prepare_sources(
            workers,
            batch,
            &mut known_sources,
            &mut sources,
            ffmpeg_enabled,
            cancellation,
            &mut scratch,
        )
        .await?;
    }
    cancellation.check()?;
    // Restore first-request order without cloning probe metadata or fingerprints.
    let mut outcomes: Vec<_> = (0..identities.len()).map(|_| None).collect();
    for (path, outcome) in sources {
        let index = identities[&path];
        outcomes[index] = Some(outcome.map_err(|error| format!("{}: {error}", path.display())));
    }
    let mut records = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        match outcome.context("External file preparation omitted a source")? {
            Ok(record) => {
                records.push(Some(result.records.len()));
                result.records.push(record);
            }
            Err(error) => {
                records.push(None);
                result.report(error);
            }
        }
    }
    result.occurrences = order
        .into_iter()
        .filter_map(|index| records[index])
        .collect();
    result.rejected = requested - result.occurrences.len();
    cancellation.check()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_audio::MediaInfo;
    use std::path::Path;

    fn wav(path: &Path) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..80 {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn exact_files_keep_order_duplicates_and_short_audio_without_expansion() {
        let workers = &BlockingPool::new(4);
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.wav");
        let second = directory.path().join("second.wav");
        wav(&first);
        wav(&second);
        let nested = directory.path().join("directory.wav");
        std::fs::create_dir(&nested).unwrap();
        wav(&nested.join("hidden.wav"));
        let cue = directory.path().join("tracks.cue");
        let playlist = directory.path().join("tracks.m3u");
        std::fs::write(
            &cue,
            "FILE first.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        )
        .unwrap();
        std::fs::write(&playlist, "second.wav\n").unwrap();

        let mut lookups = Vec::new();
        let result = futures::executor::block_on(prepare_external_admission(
            workers,
            vec![
                second.clone(),
                nested,
                first.clone(),
                cue,
                first.with_file_name("./first.wav"),
                playlist,
                directory.path().join("missing.wav"),
            ],
            |paths| {
                let known = vec![None; paths.len()];
                lookups.extend(paths);
                std::future::ready(Ok(known))
            },
            false,
            &ScanCancellation::default(),
            |_| {},
        ))
        .unwrap();
        let queued: Vec<_> = result
            .occurrences
            .iter()
            .map(|&index| &result.records[index].path)
            .collect();
        assert_eq!(queued, vec![&second, &first, &first]);
        assert_eq!(result.records.len(), 2);
        assert_eq!(lookups, vec![second, first]);
        assert_eq!(result.rejected, 4);
        assert_eq!(result.errors.len(), 4);
        assert!(
            result
                .records
                .iter()
                .all(|record| record.media.duration.unwrap() < 1.0)
        );
    }

    #[test]
    fn exact_probe_reuses_unchanged_cache_and_preserves_logical_symlink_path() {
        let workers = &BlockingPool::new(4);
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("not-decodable.wav");
        std::fs::write(&original, b"cached encoded content").unwrap();
        #[cfg(unix)]
        let path = {
            let path = directory.path().join("logical.wav");
            std::os::unix::fs::symlink(&original, &path).unwrap();
            path
        };
        #[cfg(not(unix))]
        let path = original;
        let metadata = std::fs::metadata(&path).unwrap();
        let known = KnownFile {
            size: metadata.len(),
            modified_ns: super::super::source::cache_modified_ns(metadata.modified().ok()),
            fingerprint: Some(blake3::hash(b"cached encoded content").to_hex().to_string()),
            media: Some(MediaInfo {
                title: "Cached title".into(),
                artist: String::new(),
                album: String::new(),
                duration: Some(0.01),
                codec: "pcm".into(),
                channels: 1,
                sample_rate: 8_000,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            }),
            cue: None,
        };

        let result = futures::executor::block_on(prepare_external_admission(
            workers,
            vec![path.clone(), path.clone()],
            |paths| {
                assert_eq!(paths, vec![path.clone()]);
                std::future::ready(Ok(vec![Some(known.clone())]))
            },
            false,
            &ScanCancellation::default(),
            |_| {},
        ))
        .unwrap();
        assert_eq!(result.records[0].path, path);
        assert_eq!(result.records[0].media.title, "Cached title");
        assert_eq!(result.occurrences, vec![0, 0]);
        assert_eq!(result.rejected, 0);
    }

    #[test]
    fn cache_batches_and_invalid_diagnostics_remain_bounded() {
        let workers = &BlockingPool::new(4);
        let directory = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..SOURCE_BATCH_SIZE + 3)
            .map(|index| {
                let path = directory.path().join(format!("{index}.wav"));
                wav(&path);
                path
            })
            .collect();
        let mut paths = files.clone();
        paths.extend(files.clone());
        paths.extend(
            (0..MAX_REPORTED_SCAN_ERRORS + 10)
                .map(|index| directory.path().join(format!("{index}.m3u"))),
        );

        let mut lookup_count = 0;
        let mut batches = 0;
        let result = futures::executor::block_on(prepare_external_admission(
            workers,
            paths,
            |paths| {
                assert!(paths.len() <= SOURCE_BATCH_SIZE);
                lookup_count += paths.len();
                batches += 1;
                std::future::ready(Ok(vec![None; paths.len()]))
            },
            false,
            &ScanCancellation::default(),
            |_| {},
        ))
        .unwrap();
        assert_eq!(lookup_count, files.len());
        assert_eq!(batches, 2);
        assert_eq!(result.records.len(), files.len());
        assert_eq!(result.occurrences.len(), files.len() * 2);
        assert_eq!(result.rejected, MAX_REPORTED_SCAN_ERRORS + 10);
        assert_eq!(result.errors.len(), MAX_REPORTED_SCAN_ERRORS);
    }

    #[test]
    fn cancelled_exact_admission_returns_no_partial_result() {
        let workers = &BlockingPool::new(4);
        let token = ScanCancellation::default();
        token.cancel();
        assert!(
            futures::executor::block_on(prepare_external_admission(
                workers,
                vec!["/missing.wav".into()],
                |_| std::future::ready(Ok(Vec::new())),
                false,
                &token,
                |_| {},
            ))
            .is_err()
        );
    }
}
