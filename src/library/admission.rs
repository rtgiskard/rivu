use super::paths::{is_audio_path, logical_path};
use super::scan::MAX_REPORTED_SCAN_ERRORS;
use super::source::{HASH_CHUNK_BYTES, SOURCE_BATCH_SIZE, ScanRecord, prepare_source_batch};
use super::{KnownFile, ScanCancellation};
use crate::execution::BlockingExecutor;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

/// Prepared ordinary sources and their requested queue occurrences. No database
/// writes, traversal, playlist/CUE expansion or duration filtering occur here.
#[derive(Debug, Default)]
pub(crate) struct ExternalAdmission {
    records: Vec<ScanRecord>,
    occurrences: Vec<usize>,
    pub(crate) rejected: usize,
    pub(crate) errors: Vec<String>,
}

impl ExternalAdmission {
    pub(crate) fn records(&self) -> &[ScanRecord] {
        &self.records
    }

    pub(crate) fn occurrences(&self) -> &[usize] {
        &self.occurrences
    }

    #[cfg(test)]
    pub(crate) fn from_test_records(records: Vec<ScanRecord>, occurrences: Vec<usize>) -> Self {
        assert!(occurrences.iter().all(|&index| index < records.len()));
        assert!(
            records
                .iter()
                .all(|record| logical_path(&record.path).unwrap() == record.path)
        );
        Self {
            records,
            occurrences,
            ..Self::default()
        }
    }

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
    workers: &BlockingExecutor,
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
    let total = unique.len();
    drop(identities);
    let mut records = Vec::with_capacity(total);
    let mut scratch = vec![0; HASH_CHUNK_BYTES];
    let mut unique = unique.into_iter();
    loop {
        cancellation.check()?;
        let batch: Vec<_> = unique.by_ref().take(SOURCE_BATCH_SIZE).collect();
        if batch.is_empty() {
            break;
        }
        progress(rivu_protocol::model::ScanProgress {
            phase: rivu_protocol::model::ScanPhase::ReadingMetadata,
            processed: records.len(),
            total: Some(total),
            errors: result.errors.len(),
            path: batch.first().cloned(),
        });
        for outcome in prepare_source_batch(
            workers,
            batch,
            &mut known_sources,
            ffmpeg_enabled,
            (cancellation, None),
            &mut scratch,
        )
        .await?
        {
            match outcome {
                Ok(record) => {
                    records.push(Some(result.records.len()));
                    result.records.push(record);
                }
                Err((path, error)) => {
                    records.push(None);
                    result.report(format!("{}: {error}", path.display()));
                }
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
        crate::test_support::write_i16_wav(path, 1, 8_000, [0; 80]).unwrap();
    }

    #[test]
    fn exact_files_keep_order_duplicates_and_short_audio_without_expansion() {
        let workers = &BlockingExecutor::new(4);
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
        let workers = &BlockingExecutor::new(4);
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
        let workers = &BlockingExecutor::new(4);
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
    fn source_batches_preserve_order_across_open_probe_failures_and_duplicates() {
        let workers = &BlockingExecutor::new(4);
        let directory = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..SOURCE_BATCH_SIZE + 3)
            .map(|index| {
                let path = directory.path().join(format!("{index}.wav"));
                wav(&path);
                path
            })
            .collect();
        let missing = directory.path().join("missing.wav");
        let broken = directory.path().join("broken.wav");
        std::fs::write(&broken, b"not encoded audio").unwrap();
        let mut paths = vec![missing.clone(), files[3].clone(), broken.clone()];
        paths.extend(files.clone());
        paths.extend([
            broken.clone(),
            files[0].clone(),
            missing.clone(),
            files[SOURCE_BATCH_SIZE + 1].clone(),
        ]);
        let expected: Vec<_> = paths
            .iter()
            .filter(|path| **path != missing && **path != broken)
            .cloned()
            .collect();
        let result = futures::executor::block_on(prepare_external_admission(
            workers,
            paths,
            |paths| std::future::ready(Ok(vec![None; paths.len()])),
            false,
            &ScanCancellation::default(),
            |_| {},
        ))
        .unwrap();
        let queued: Vec<_> = result
            .occurrences
            .iter()
            .map(|&index| result.records[index].path.clone())
            .collect();
        assert_eq!(queued, expected);
        let mut first_requests = vec![&files[3]];
        first_requests.extend(files.iter().filter(|path| **path != files[3]));
        assert_eq!(
            result
                .records
                .iter()
                .map(|record| &record.path)
                .collect::<Vec<_>>(),
            first_requests
        );
        assert_eq!(result.rejected, 4);
        assert_eq!(result.errors.len(), 2);
        assert!(result.errors[0].contains("missing.wav"));
        assert!(result.errors[1].contains("broken.wav"));
    }

    #[test]
    fn failed_lookup_batch_does_not_discard_later_sources_or_retry_duplicate_identities() {
        let workers = &BlockingExecutor::new(4);
        let directory = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..SOURCE_BATCH_SIZE + 3)
            .map(|index| {
                let path = directory.path().join(format!("{index}.wav"));
                wav(&path);
                path
            })
            .collect();
        let mut paths = files.clone();
        paths.push(files[0].clone());
        paths.extend_from_slice(&files[SOURCE_BATCH_SIZE..]);
        let mut fail_lookup = true;
        let result = futures::executor::block_on(prepare_external_admission(
            workers,
            paths,
            |paths| {
                std::future::ready(if std::mem::take(&mut fail_lookup) {
                    Err(anyhow::anyhow!("source cache unavailable"))
                } else {
                    Ok(vec![None; paths.len()])
                })
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
        assert_eq!(
            queued,
            files[SOURCE_BATCH_SIZE..]
                .iter()
                .chain(&files[SOURCE_BATCH_SIZE..])
                .collect::<Vec<_>>()
        );
        assert_eq!(result.records.len(), 3);
        assert_eq!(result.rejected, SOURCE_BATCH_SIZE + 1);
        assert_eq!(result.errors.len(), SOURCE_BATCH_SIZE);
    }

    #[test]
    fn cancelled_exact_admission_returns_no_partial_result() {
        let workers = &BlockingExecutor::new(4);
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
