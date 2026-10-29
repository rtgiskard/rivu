use super::scan::{
    MAX_RECORDED_SCAN_UNCERTAINTIES, MAX_RECORDED_SKIPPED_DIRECTORIES, MAX_REPORTED_SCAN_ERRORS,
};
use super::source::{UNCACHEABLE_MODIFIED_NS, cache_modified_ns};
use super::*;
use anyhow::{Context, Result};
use rivu_protocol::model::{CueSegment, PlaylistExportTrack, ScanPhase, Track};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

fn known_source(
    known: &[(PathBuf, KnownFile)],
) -> impl FnMut(&[PathBuf]) -> Result<Vec<Option<KnownFile>>> + '_ {
    move |paths| {
        Ok(paths
            .iter()
            .map(|path| {
                known
                    .iter()
                    .filter(|(key, _)| key == path)
                    .min_by_key(|(_, file)| file.cue.is_some())
                    .map(|(_, file)| file.clone())
            })
            .collect())
    }
}

fn scan_paths(
    paths: &[PathBuf],
    known: &[(PathBuf, KnownFile)],
    ffmpeg_enabled: bool,
) -> Result<ScanResult> {
    let library_roots: Vec<_> = paths
        .iter()
        .map(|path| logical_path(path))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|path| {
            if path.is_dir() {
                path
            } else {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.clone())
            }
        })
        .collect();
    super::scan_paths(
        paths,
        known_source(known),
        ffmpeg_enabled,
        &library_roots,
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )
}

async fn scan_uncached_paths(
    workers: &crate::execution::BlockingExecutor,
    paths: &[PathBuf],
    roots: &[PathBuf],
) -> Result<ScanResult> {
    super::scan_paths_with_cues(
        workers,
        paths,
        |paths| std::future::ready(Ok(vec![None; paths.len()])),
        super::ScanSettings {
            ffmpeg_enabled: false,
            library_roots: roots,
            max_depth: crate::config::Config::default().scan_max_depth,
        },
        (&ScanCancellation::default(), None),
        |_, _, _, _, _| {},
    )
    .await
}

#[test]
fn source_lookup_batches_are_bounded_and_cover_only_encountered_sources() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let count = 2 * super::source::SOURCE_BATCH_SIZE + 1;
    for index in 0..count {
        wav(&root.join(format!("{index:03}.wav")), 75)?;
    }
    let mut batches = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let result = super::scan_paths(
        std::slice::from_ref(&root),
        |paths| {
            assert!(paths.len() <= super::source::SOURCE_BATCH_SIZE);
            batches.push(paths.len());
            for path in paths {
                assert!(path.starts_with(&root));
                assert!(
                    seen.insert(path.clone()),
                    "source must be admitted only once"
                );
            }
            Ok(vec![None; paths.len()])
        },
        false,
        std::slice::from_ref(&root),
        1,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.track_count(), count);
    assert_eq!(seen.len(), count);
    assert_eq!(
        batches,
        [
            super::source::SOURCE_BATCH_SIZE,
            super::source::SOURCE_BATCH_SIZE,
            1
        ]
    );
    Ok(())
}

#[test]
fn cue_lookup_batches_share_sources_across_sheet_batches() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let count = super::source::SOURCE_BATCH_SIZE + 1;
    for index in 0..count {
        wav(&root.join(format!("{index:03}.wav")), 75)?;
        fs::write(
            root.join(format!("{index:03}.cue")),
            format!("FILE \"{index:03}.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n"),
        )?;
    }
    fs::write(
        root.join("shared.cue"),
        "FILE \"000.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
    )?;
    let mut batches = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let result = super::scan_paths(
        std::slice::from_ref(&root),
        |paths| {
            batches.push(paths.len());
            for path in paths {
                assert!(
                    seen.insert(path.clone()),
                    "shared CUE sources must not be reprobed"
                );
            }
            Ok(vec![None; paths.len()])
        },
        false,
        std::slice::from_ref(&root),
        1,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(batches, [super::source::SOURCE_BATCH_SIZE, 1]);
    assert_eq!(seen.len(), count);
    assert_eq!(result.track_count(), 2 * count + 1);
    assert_eq!(
        result
            .tracks()
            .filter(|record| record.cue.is_none())
            .count(),
        count
    );
    assert!(result.tracks().all(|record| record.duration_filter_exempt));
    Ok(())
}

#[test]
fn successful_cue_sources_and_segments_bypass_duration_admission() -> Result<()> {
    for frames in [150, 225] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().to_path_buf();
        let audio = root.join("audio.wav");
        let sheet = root.join("album.cue");
        wav(&audio, frames)?;
        two_track_sheet(&sheet)?;
        let store = crate::store::Store::open(Path::new(":memory:"))?;
        let mut result = scan_paths(std::slice::from_ref(&root), &[], false)?;
        result.min_duration_secs = 10;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.track_count(), 3);
        assert!(result.tracks().all(|track| track.duration_filter_exempt));
        let impacts = store.apply_scan(&result)?;
        assert_eq!(impacts.skipped_short_tracks, 0);
        assert_eq!(store.track_count()?, 3);
        let whole = store
            .track_id_for_identity(crate::store::TrackIdentity::File(&audio))?
            .context("CUE-referenced whole source must be admitted")?;
        let first = store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))?
            .context("Short CUE segment must be admitted")?;
        let second = store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 2))?
            .context("Final CUE segment must be admitted")?;
        assert_eq!(
            store.track(whole)?.unwrap().duration,
            Some(f64::from(frames) / 75.0)
        );
        assert_eq!(store.track(first)?.unwrap().duration, Some(1.0));
        fs::write(
            &sheet,
            "TITLE \"Refreshed album\"\nFILE \"audio.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    TITLE \"Refreshed second\"\n    INDEX 01 00:01:00\n",
        )?;
        let mut rescanned = scan_paths(std::slice::from_ref(&root), &[], false)?;
        rescanned.min_duration_secs = 20;
        let impacts = store.apply_scan(&rescanned)?;
        assert_eq!(impacts.skipped_short_tracks, 0);
        assert_eq!(
            store.track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 2))?,
            Some(second)
        );
        let track = store.track(second)?.unwrap();
        assert!(!track.missing);
        assert_eq!(track.title, "Refreshed second");
        assert_eq!(track.album, "Refreshed album");
        assert_eq!(track.duration, Some(f64::from(frames) / 75.0 - 1.0));
    }
    Ok(())
}

#[test]
fn cue_requests_publish_only_successful_sources_outside_the_request_scope() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let sheets = root.join("sheets");
    let sources = root.join("sources");
    fs::create_dir(&sheets)?;
    fs::create_dir(&sources)?;
    let valid_audio = sources.join("valid.wav");
    let failed_audio = sources.join("failed.wav");
    wav(&valid_audio, 75)?;
    wav(&failed_audio, 75)?;
    let valid_sheet = sheets.join("valid.cue");
    fs::write(
        &valid_sheet,
        "FILE \"../sources/valid.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
    )?;
    let failed_sheet = sheets.join("failed.cue");
    fs::write(
        &failed_sheet,
        "FILE \"../sources/failed.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:02:00\n",
    )?;
    for (direct_sheets, discover_failed_audio) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let mut paths = if direct_sheets {
            vec![valid_sheet.clone(), failed_sheet.clone()]
        } else {
            vec![sheets.clone()]
        };
        if discover_failed_audio {
            paths.push(failed_audio.clone());
        }
        let mut result = super::scan_paths(
            &paths,
            |paths| Ok(vec![None; paths.len()]),
            false,
            std::slice::from_ref(&root),
            1,
            &ScanCancellation::default(),
            |_, _, _, _, _| {},
        )?;
        assert_eq!(result.errors.len(), 1);
        assert_eq!(
            result.track_count(),
            if discover_failed_audio { 3 } else { 2 }
        );
        assert_eq!(
            result
                .tracks()
                .filter(|track| track.duration_filter_exempt)
                .count(),
            2
        );
        assert_eq!(
            result
                .tracks()
                .filter(|track| track.source.path == failed_audio)
                .count(),
            usize::from(discover_failed_audio)
        );
        result.min_duration_secs = 10;
        let store = crate::store::Store::open(Path::new(":memory:"))?;
        let impacts = store.apply_scan(&result)?;
        assert_eq!(
            impacts.skipped_short_tracks,
            usize::from(discover_failed_audio)
        );
        assert_eq!(store.track_count()?, 2);
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::File(&valid_audio))?
                .is_some()
        );
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::Cue(&valid_sheet, 1))?
                .is_some()
        );
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::File(&failed_audio))?
                .is_none()
        );
    }
    Ok(())
}

#[test]
fn duration_skips_are_counted_separately_from_original_scan_errors() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    wav(&root.join("short.wav"), 75)?;
    fs::write(root.join("broken.wav"), b"not audio")?;
    let mut result = scan_paths(&[root], &[], false)?;
    result.min_duration_secs = 2;
    assert_eq!(result.errors.len(), 1);
    let errors = result.errors.clone();
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    let impacts = store.apply_scan(&result)?;
    assert_eq!(impacts.skipped_short_tracks, 1);
    assert_eq!(result.errors, errors);
    assert_eq!(store.track_count()?, 0);
    Ok(())
}

#[test]
fn new_and_retargeted_cue_sheets_reuse_known_sources_outside_request_scope() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let sources = directory.path().join("sources");
    let sheets = directory.path().join("sheets");
    fs::create_dir_all(&sources)?;
    fs::create_dir_all(&sheets)?;
    let first = sources.join("first.wav");
    let second = sources.join("second.wav");
    wav(&first, 150)?;
    wav(&second, 150)?;
    let mut baseline = scan_paths(&[first, second], &[], false)?;
    assert!(baseline.errors.is_empty(), "{:?}", baseline.errors);
    for record in &mut baseline.records {
        let record = std::sync::Arc::get_mut(record).expect("unshared ordinary source");
        record.media.artist = format!(
            "Cached {}",
            record.path.file_stem().unwrap().to_str().unwrap()
        );
        record.fingerprint = Some(
            blake3::hash(record.media.artist.as_bytes())
                .to_hex()
                .to_string(),
        );
    }
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&baseline)?;
    let sheet = sheets.join("album.cue");
    for (index, name) in ["first", "second"].into_iter().enumerate() {
        fs::write(
            &sheet,
            format!(
                "FILE \"../sources/{name}.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n"
            ),
        )?;
        // Exercise both explicit sheet and tiny directory requests. Neither
        // may discard reusable whole-source metadata beyond the request root.
        let requested = if index == 0 {
            sheet.clone()
        } else {
            sheets.clone()
        };
        let scan = super::scan_paths(
            &[requested],
            |paths| paths.iter().map(|path| store.known_source(path)).collect(),
            false,
            &[directory.path().to_path_buf()],
            crate::config::Config::default().scan_max_depth,
            &ScanCancellation::default(),
            |_, _, _, _, _| {},
        )?;
        assert!(scan.errors.is_empty(), "{:?}", scan.errors);
        assert_eq!(scan.track_count(), 2);
        assert_eq!(
            scan.tracks().next().unwrap().media.artist,
            format!("Cached {name}")
        );
        assert_eq!(
            scan.tracks().next().unwrap().source.fingerprint,
            baseline
                .records
                .iter()
                .find(|record| record.path == scan.tracks().next().unwrap().source.path)
                .unwrap()
                .fingerprint,
        );
        store.apply_scan(&scan)?;
    }
    Ok(())
}

fn scan_paths_at_depth(
    paths: &[PathBuf],
    known: &[(PathBuf, KnownFile)],
    ffmpeg_enabled: bool,
    max_depth: u32,
    progress: impl FnMut(ScanPhase, usize, Option<usize>, usize, Option<&Path>),
) -> Result<ScanResult> {
    let library_roots: Vec<_> = paths
        .iter()
        .map(|path| logical_path(path))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|path| {
            if path.is_dir() {
                path
            } else {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.clone())
            }
        })
        .collect();
    super::scan_paths(
        paths,
        known_source(known),
        ffmpeg_enabled,
        &library_roots,
        max_depth,
        &ScanCancellation::default(),
        progress,
    )
}

// Mono PCM WAVs with exactly 100 samples per CD frame, no audio device.
fn wav(path: &Path, frames: u32) -> Result<()> {
    let data_len = frames * 100 * 2;
    let mut file = File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_len).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&7500_u32.to_le_bytes())?;
    file.write_all(&15000_u32.to_le_bytes())?;
    file.write_all(&2_u16.to_le_bytes())?;
    file.write_all(&16_u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    file.write_all(&vec![0; data_len as usize])?;
    Ok(())
}

fn two_track_sheet(path: &Path) -> Result<()> {
    fs::write(
        path,
        "TITLE \"Album\"\nPERFORMER \"Sheet artist\"\nFILE \"audio.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"First\"\n    PERFORMER \"Track artist\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 00 00:00:60\n    INDEX 01 00:01:00\n",
    )?;
    Ok(())
}

#[test]
fn cancellation_is_shared_and_discards_discovery_or_accumulated_records() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    wav(&root.join("first.wav"), 150)?;
    wav(&root.join("second.wav"), 150)?;
    for cancel_phase in [ScanPhase::Discovering, ScanPhase::ReadingMetadata] {
        let cancellation = ScanCancellation::default();
        let owner = cancellation.clone();
        assert!(owner.check().is_ok());
        let outcome = super::scan_paths(
            std::slice::from_ref(&root),
            |paths| Ok(vec![None; paths.len()]),
            false,
            std::slice::from_ref(&root),
            1,
            &cancellation,
            |phase, processed, _, _, path| {
                if phase == cancel_phase && processed > 0 && path.is_some() {
                    owner.cancel();
                }
            },
        );
        assert!(owner.is_cancelled());
        assert!(cancellation.check().is_err());
        assert!(
            outcome.is_err(),
            "cancelled scan must not expose partial records"
        );
    }
    let cancellation = ScanCancellation::default();
    cancellation.cancel();
    let mut reported_progress = false;
    assert!(
        super::scan_paths(
            std::slice::from_ref(&root),
            |paths| Ok(vec![None; paths.len()]),
            false,
            std::slice::from_ref(&root),
            1,
            &cancellation,
            |_, _, _, _, _| reported_progress = true,
        )
        .is_err()
    );
    assert!(!reported_progress);
    Ok(())
}

#[test]
fn async_source_lookup_releases_the_only_blocking_slot() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let audio = root.join("audio.wav");
    wav(&audio, 150)?;
    let workers = &crate::execution::BlockingExecutor::new(1);
    futures::executor::block_on(async {
        let cancellation = ScanCancellation::default();
        let (lookup, requested) = futures::channel::oneshot::channel();
        let (reply, response) =
            futures::channel::oneshot::channel::<Result<Vec<Option<KnownFile>>>>();
        let mut lookup = Some(lookup);
        let mut response = Some(response);
        let paths = [root.clone()];
        let scan = super::scan_paths_with_cues(
            workers,
            &paths,
            |paths| {
                assert_eq!(paths.as_slice(), std::slice::from_ref(&audio));
                lookup.take().unwrap().send(()).unwrap();
                let response = response.take().unwrap();
                async move { response.await.context("lookup owner disconnected")? }
            },
            super::scan::ScanSettings {
                ffmpeg_enabled: false,
                library_roots: &paths,
                max_depth: 1,
            },
            (&cancellation, None),
            |_, _, _, _, _| {},
        );
        let owner = async {
            requested.await?;
            // This cannot run if an entire scan closure occupies the sole slot
            // while synchronously waiting for its source lookup reply.
            rivu_protocol::transport::timeout(Duration::from_secs(5), workers.execute(|| ()))
                .await??;
            reply.send(Ok(vec![None])).unwrap();
            anyhow::Ok(())
        };
        let (result, owner) = futures::join!(scan, owner);
        owner?;
        let result = result?;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.track_count(), 1);
        assert_eq!(result.records[0].path, audio);
        anyhow::Ok(())
    })
}

#[test]
fn cancellation_wakes_every_async_waiter_and_remains_observable() {
    futures::executor::block_on(async {
        let cancellation = ScanCancellation::default();
        let first = cancellation.clone();
        let second = cancellation.clone();
        let (first, second, ()) =
            rivu_protocol::transport::timeout(Duration::from_secs(5), async {
                futures::join!(
                    async move {
                        first.cancelled().await;
                        first.check()
                    },
                    async move {
                        second.cancelled().await;
                        second.check()
                    },
                    async {
                        futures_lite::future::yield_now().await;
                        cancellation.cancel();
                    },
                )
            })
            .await
            .unwrap();
        assert!(first.is_err());
        assert!(second.is_err());
        rivu_protocol::transport::timeout(Duration::from_secs(5), cancellation.cancelled())
            .await
            .unwrap();
        assert!(cancellation.is_cancelled());
    })
}

#[test]
fn cancellation_during_a_hash_read_stops_at_the_chunk_boundary() {
    struct CancellingReader {
        cancellation: ScanCancellation,
        reads: usize,
    }
    impl std::io::Read for CancellingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.reads == 2 {
                self.cancellation.cancel();
            }
            buffer.fill(1);
            Ok(buffer.len())
        }
    }
    let cancellation = ScanCancellation::default();
    let mut reader = CancellingReader {
        cancellation: cancellation.clone(),
        reads: 0,
    };
    let mut scratch = [0; 128 * 1024];
    let mut hasher = blake3::Hasher::new();
    let first = super::source::hash_chunk(&mut reader, &mut scratch, &mut hasher, &cancellation);
    assert_eq!(first.unwrap(), Some(scratch.len()));
    let first_hash = hasher.finalize();
    assert!(
        super::source::hash_chunk(&mut reader, &mut scratch, &mut hasher, &cancellation).is_err()
    );
    assert_eq!(
        hasher.finalize(),
        first_hash,
        "cancelled read must not enter the fingerprint"
    );
    assert_eq!(reader.reads, 2);
}

#[test]
fn bounded_playlist_reads_enforce_capacity_and_cancellation_boundaries() -> Result<()> {
    futures::executor::block_on(async {
        use super::input::{MAX_CUE_BYTES, read_chunks_async, read_input_async};
        let workers = &crate::execution::BlockingExecutor::new(4);

        let directory = tempfile::tempdir()?;
        let sheet = directory.path().join("album.cue");
        let cancellation = ScanCancellation::default();
        let bytes = vec![b' '; MAX_CUE_BYTES];
        fs::write(&sheet, &bytes)?;
        assert_eq!(
            read_input_async(workers, &sheet, MAX_CUE_BYTES, &cancellation).await?,
            bytes
        );
        File::options()
            .write(true)
            .open(&sheet)?
            .set_len(MAX_CUE_BYTES as u64 + 1)?;
        assert!(
            read_input_async(workers, &sheet, MAX_CUE_BYTES, &cancellation)
                .await
                .is_err()
        );
        // A source that grows after metadata was checked is still bounded per chunk.
        let grown = std::io::Cursor::new(vec![b' '; MAX_CUE_BYTES + 1]);
        assert!(
            read_chunks_async(workers, grown, Vec::new(), MAX_CUE_BYTES, &cancellation)
                .await
                .is_err()
        );

        struct CancellingReader {
            cancellation: ScanCancellation,
            reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            at_eof: bool,
        }
        impl std::io::Read for CancellingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.cancellation.cancel();
                if self.at_eof {
                    Ok(0)
                } else {
                    buffer.fill(b' ');
                    Ok(buffer.len())
                }
            }
        }
        for at_eof in [false, true] {
            let cancellation = ScanCancellation::default();
            let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let reader = CancellingReader {
                cancellation: cancellation.clone(),
                reads: reads.clone(),
                at_eof,
            };
            assert!(
                read_chunks_async(workers, reader, Vec::new(), MAX_CUE_BYTES, &cancellation)
                    .await
                    .is_err()
            );
            assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 1);
            // Cancellation wins before metadata lookup or decoding malformed input.
            assert!(
                read_input_async(
                    workers,
                    &directory.path().join("absent.cue"),
                    MAX_CUE_BYTES,
                    &cancellation,
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("cancelled")
            );
            assert!(
                super::cue::parse_input(&[0xff], &sheet, &cancellation)
                    .unwrap_err()
                    .to_string()
                    .contains("cancelled")
            );
        }
        Ok(())
    })
}

#[test]
fn oversized_scan_cue_is_a_local_error_preserving_owned_tracks() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    wav(&root.join("audio.wav"), 225)?;
    let sheet = root.join("album.cue");
    two_track_sheet(&sheet)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
    let ids = [
        store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))?
            .unwrap(),
        store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 2))?
            .unwrap(),
    ];
    store.set_favorite(&ids, true)?;
    store.edit_track(ids[0], Some("Owned title"), None, None, None)?;
    // A sparse 32 MiB sheet must be rejected by size, before decoding/parsing.
    File::options()
        .write(true)
        .open(&sheet)?
        .set_len(32 * 1024 * 1024)?;
    let result = scan_paths(&[root], &[], false)?;
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.uncertainties, [ScanUncertainty::CueSheet(sheet)]);
    assert!(result.tracks().all(|track| !track.duration_filter_exempt));
    assert_eq!(result.track_count(), 1);
    store.apply_scan(&result)?;
    store.remove_missing_tracks()?;
    for id in ids {
        let track = store.track(id)?.unwrap();
        assert!(track.favorite && !track.missing);
    }
    assert_eq!(store.track(ids[0])?.unwrap().title, "Owned title");
    Ok(())
}

#[cfg(unix)]
#[test]
fn scan_rejects_fifo_sheets_before_or_after_discovery_without_losing_owned_tracks() -> Result<()> {
    for (replace_after_discovery, explicit_sheet) in [(false, false), (false, true), (true, false)]
    {
        let directory = tempfile::tempdir()?;
        let root = directory.path().to_path_buf();
        wav(&root.join("audio.wav"), 225)?;
        let sheet = root.join("album.cue");
        two_track_sheet(&sheet)?;
        let store = crate::store::Store::open(Path::new(":memory:"))?;
        store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
        let ids = [
            store
                .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))?
                .unwrap(),
            store
                .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 2))?
                .unwrap(),
        ];
        let replace = |sheet: &Path| {
            fs::remove_file(sheet).unwrap();
            let name = std::ffi::CString::new(sheet.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        };
        if !replace_after_discovery {
            replace(&sheet);
        }
        let requested = if explicit_sheet { sheet.clone() } else { root };
        let worker_sheet = sheet.clone();
        let (completed, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result =
                scan_paths_at_depth(&[requested], &[], false, 1, |phase, processed, _, _, _| {
                    if replace_after_discovery
                        && phase == ScanPhase::ReadingMetadata
                        && processed == 0
                    {
                        replace(&worker_sheet);
                    }
                });
            completed.send(result).unwrap();
        });
        // Do not join on timeout: a regression must fail rather than hang the suite.
        let result = receive.recv_timeout(Duration::from_secs(2))??;
        worker.join().unwrap();
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.uncertainties, [ScanUncertainty::CueSheet(sheet)]);
        store.apply_scan(&result)?;
        store.remove_missing_tracks()?;
        for id in ids {
            assert!(!store.track(id)?.unwrap().missing);
        }
    }
    Ok(())
}

#[test]
fn fifo_cue_sources_are_local_errors_and_allow_subsequent_scans() -> Result<()> {
    for enabled in [false, true] {
        for replace_after_discovery in [false, true] {
            let directory = tempfile::tempdir()?;
            let root = directory.path().to_path_buf();
            let audio = root.join("audio.wav");
            let good = root.join("good.wav");
            let sheet = root.join("album.cue");
            wav(&audio, 225)?;
            wav(&good, 150)?;
            two_track_sheet(&sheet)?;
            let full = scan_paths(&[audio.clone(), good.clone()], &[], enabled)?;
            let mut known = known_records(&full);
            let baseline = scan_paths(std::slice::from_ref(&root), &[], enabled)?;
            let store = crate::store::Store::open(Path::new(":memory:"))?;
            store.apply_scan(&baseline)?;
            let ids = [
                store
                    .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))?
                    .unwrap(),
                store
                    .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 2))?
                    .unwrap(),
            ];
            let replace = |path: &Path| {
                fs::remove_file(path).unwrap();
                let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            };
            if !replace_after_discovery {
                replace(&audio);
                // Even a matching cache entry must not authorize a nonregular source.
                let metadata = fs::metadata(&audio)?;
                let (_, source) = known.iter_mut().find(|(path, _)| *path == audio).unwrap();
                source.size = metadata.len();
                source.modified_ns = cache_modified_ns(metadata.modified().ok());
            }
            let worker_audio = audio.clone();
            let worker_root = root.clone();
            let (completed, receive) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let run = || -> Result<(ScanResult, ScanResult, bool)> {
                    let mut terminal = false;
                    let result = scan_paths_at_depth(
                        std::slice::from_ref(&worker_root),
                        &known,
                        enabled,
                        1,
                        |phase, processed, total, _, path| {
                            if replace_after_discovery
                                && phase == ScanPhase::ReadingMetadata
                                && processed == 0
                            {
                                replace(&worker_audio);
                            }
                            terminal |= phase == ScanPhase::ReadingMetadata
                                && total == Some(processed)
                                && path.is_none();
                        },
                    )?;
                    fs::remove_file(&worker_audio)?;
                    wav(&worker_audio, 225)?;
                    let next = scan_paths(&[worker_root], &known, enabled)?;
                    Ok((result, next, terminal))
                };
                completed.send(run()).unwrap();
            });
            // Do not join on timeout: an unopened FIFO must never hang the suite.
            let (result, next, terminal) = receive.recv_timeout(Duration::from_secs(5))??;
            worker.join().unwrap();
            assert!(terminal);
            assert!(!result.errors.is_empty());
            assert!(
                result
                    .errors
                    .iter()
                    .all(|error| error.contains("not an ordinary file"))
            );
            assert_eq!(result.track_count(), 1);
            assert_eq!(result.records[0].path, good);
            assert!(result.tracks().all(|track| !track.duration_filter_exempt));
            assert!(
                result
                    .uncertainties
                    .contains(&ScanUncertainty::CueSheet(sheet))
            );
            assert!(result.uncertainties.contains(&ScanUncertainty::File(audio)));
            store.apply_scan(&result)?;
            store.remove_missing_tracks()?;
            for id in ids {
                assert!(!store.track(id)?.unwrap().missing);
            }
            assert!(next.errors.is_empty(), "{:?}", next.errors);
            assert_eq!(next.track_count(), 4);
        }
    }
    Ok(())
}

#[test]
fn regular_audio_symlink_is_scanned_hashed_and_used_by_cue() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let actual = tempfile::tempdir()?;
    let target = actual.path().join("source.wav");
    wav(&target, 225)?;
    let audio = directory.path().join("audio.wav");
    std::os::unix::fs::symlink(&target, &audio)?;
    two_track_sheet(&directory.path().join("album.cue"))?;
    let expected = blake3::hash(&fs::read(target)?).to_hex().to_string();
    for enabled in [false, true] {
        let result = scan_paths(
            &[directory.path().to_path_buf(), audio.clone()],
            &[],
            enabled,
        )?;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.track_count(), 3);
        assert!(result.tracks().all(|record| {
            record.source.path == audio
                && record.source.fingerprint.as_deref() == Some(expected.as_str())
        }));
        assert_eq!(
            result
                .tracks()
                .filter(|record| record.cue.is_some())
                .count(),
            2
        );
    }
    Ok(())
}

#[test]
fn opened_scan_source_keeps_metadata_and_hash_when_its_symlink_is_retargeted() -> Result<()> {
    for enabled in [false, true] {
        for cue in [false, true] {
            let directory = tempfile::tempdir()?;
            let root = directory.path().to_path_buf();
            let first = root.join("first.wav");
            let second = root.join("second.wav");
            let audio = root.join("audio.wav");
            let sheet = root.join("album.cue");
            wav(&first, 225)?;
            wav(&second, 75)?;
            std::os::unix::fs::symlink(&first, &audio)?;
            two_track_sheet(&sheet)?;
            let metadata = fs::metadata(&first)?;
            let expected = blake3::hash(&fs::read(&first)?).to_hex().to_string();
            #[cfg(feature = "ffmpeg")]
            if enabled {
                // Exercise loaded-runtime WAVE inspection as well as native-only opens.
                rivu_audio::probe_details(&first, true, true)?;
            }
            let requested = if cue { sheet } else { audio.clone() };
            let mut lookups = 0;
            let result = super::scan_paths(
                &[requested],
                |paths| {
                    assert_eq!(paths, std::slice::from_ref(&audio));
                    lookups += 1;
                    // The scanner has opened the original target before this lookup.
                    fs::remove_file(&audio)?;
                    std::os::unix::fs::symlink(&second, &audio)?;
                    Ok(vec![None])
                },
                enabled,
                &[root],
                1,
                &ScanCancellation::default(),
                |_, _, _, _, _| {},
            )?;
            assert_eq!(lookups, 1);
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            assert_eq!(result.track_count(), if cue { 3 } else { 1 });
            assert!(result.tracks().all(|record| {
                record.source.path == audio
                    && record.source.size == metadata.len()
                    && record.source.modified_ns == cache_modified_ns(metadata.modified().ok())
                    && record.source.fingerprint.as_deref() == Some(expected.as_str())
            }));
            let duration: f64 = result
                .tracks()
                .map(|record| record.media.duration.unwrap())
                .sum();
            assert_eq!(duration, if cue { 6.0 } else { 3.0 });
            assert_ne!(blake3::hash(&fs::read(&audio)?).to_hex().as_str(), expected);
        }
    }
    Ok(())
}

#[test]
fn in_place_source_changes_fail_scans_instead_of_mixing_versions() -> Result<()> {
    for enabled in [false, true] {
        for (cached, frames) in [(false, 75), (true, 75), (false, 300), (true, 300)] {
            let directory = tempfile::tempdir()?;
            let root = directory.path().to_path_buf();
            let audio = root.join("audio.wav");
            wav(&audio, 225)?;
            let baseline = scan_paths(std::slice::from_ref(&audio), &[], enabled)?;
            let (_, known) = known_records(&baseline).pop().unwrap();
            let mut changed = false;
            let result = super::scan_paths(
                std::slice::from_ref(&audio),
                |paths| {
                    assert_eq!(paths, std::slice::from_ref(&audio));
                    // Rewrite the already opened inode, not a replacement pathname.
                    wav(&audio, frames)?;
                    changed = true;
                    Ok(vec![cached.then(|| known.clone())])
                },
                enabled,
                &[root],
                1,
                &ScanCancellation::default(),
                |_, _, _, _, _| {},
            )?;
            assert!(changed);
            assert!(result.track_count() == 0);
            assert_eq!(result.errors.len(), 1);
            assert!(result.errors[0].contains("changed while scanning"));
            assert_eq!(result.uncertainties, [ScanUncertainty::File(audio.clone())]);
            let next = scan_paths(std::slice::from_ref(&audio), &[], enabled)?;
            assert!(next.errors.is_empty(), "{:?}", next.errors);
            assert_eq!(
                next.records[0].media.duration,
                Some(f64::from(frames) / 75.0)
            );
            assert_eq!(
                next.records[0].fingerprint,
                Some(blake3::hash(&fs::read(&audio)?).to_hex().to_string())
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn nonregular_audio_is_a_local_error_preserving_only_its_existing_identity() -> Result<()> {
    for enabled in [false, true] {
        for replacement in ["fifo", "linked-fifo", "linked-device"] {
            for explicit in [false, true] {
                for replace_after_discovery in [false, true] {
                    let directory = tempfile::tempdir()?;
                    let root = directory.path().to_path_buf();
                    let audio = root.join("audio.wav");
                    let deleted = root.join("deleted.wav");
                    let good = root.join("good.wav");
                    wav(&audio, 150)?;
                    wav(&deleted, 100)?;
                    let baseline = scan_paths(std::slice::from_ref(&root), &[], enabled)?;
                    let mut known = known_records(&baseline);
                    let store = crate::store::Store::open(Path::new(":memory:"))?;
                    store.apply_scan(&baseline)?;
                    let id = store
                        .track_id_for_identity(crate::store::TrackIdentity::File(&audio))?
                        .unwrap();
                    let deleted_id = store
                        .track_id_for_identity(crate::store::TrackIdentity::File(&deleted))?
                        .unwrap();
                    store.set_favorite(&[id], true)?;
                    store.edit_track(id, Some("Kept title"), None, None, None)?;
                    fs::remove_file(&deleted)?;
                    wav(&good, 75)?;
                    let replace = move |path: &Path| {
                        fs::remove_file(path).unwrap();
                        if replacement == "linked-device" {
                            std::os::unix::fs::symlink("/dev/null", path).unwrap();
                        } else {
                            let fifo = if replacement == "fifo" {
                                path.to_path_buf()
                            } else {
                                path.with_file_name("pipe")
                            };
                            let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())
                                .unwrap();
                            // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
                            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                            if replacement == "linked-fifo" {
                                std::os::unix::fs::symlink(fifo, path).unwrap();
                            }
                        }
                    };
                    if !replace_after_discovery {
                        replace(&audio);
                        // A matching cache entry cannot authorize nonregular input.
                        let metadata = fs::metadata(&audio)?;
                        let (_, cached) =
                            known.iter_mut().find(|(path, _)| *path == audio).unwrap();
                        cached.size = metadata.len();
                        cached.modified_ns = cache_modified_ns(metadata.modified().ok());
                    }
                    let requested = if explicit {
                        vec![audio.clone(), good.clone(), deleted.clone()]
                    } else {
                        vec![root]
                    };
                    let worker_audio = audio.clone();
                    let (completed, receive) = std::sync::mpsc::channel();
                    let worker = std::thread::spawn(move || {
                        let mut terminal = false;
                        let result = scan_paths_at_depth(
                            &requested,
                            &known,
                            enabled,
                            1,
                            |phase, processed, total, _, path| {
                                if replace_after_discovery
                                    && phase == ScanPhase::ReadingMetadata
                                    && processed == 0
                                {
                                    replace(&worker_audio);
                                }
                                terminal |= phase == ScanPhase::ReadingMetadata
                                    && total == Some(processed)
                                    && path.is_none();
                            },
                        );
                        completed
                            .send(result.map(|result| (result, terminal)))
                            .unwrap();
                    });
                    // A nonregular source must fail, never wait for a FIFO writer.
                    let (result, terminal) = receive.recv_timeout(Duration::from_secs(5))??;
                    worker.join().unwrap();
                    assert!(terminal);
                    assert_eq!(result.errors.len(), if explicit { 2 } else { 1 });
                    assert!(
                        result
                            .errors
                            .iter()
                            .any(|error| error.contains("not an ordinary file"))
                    );
                    assert_eq!(result.uncertainties, [ScanUncertainty::File(audio.clone())]);
                    assert!(!result.scan_incomplete);
                    assert_eq!(result.track_count(), 1);
                    assert_eq!(result.records[0].path, good);
                    store.apply_scan(&result)?;
                    assert!(!store.track(id)?.unwrap().missing);
                    assert!(store.track(deleted_id)?.unwrap().missing);
                    assert!(
                        store
                            .track_id_for_identity(crate::store::TrackIdentity::File(&good))?
                            .is_some()
                    );
                    store.remove_missing_tracks()?;
                    let preserved = store.track(id)?.unwrap();
                    assert!(preserved.favorite);
                    assert_eq!(preserved.title, "Kept title");
                    assert!(store.track(deleted_id)?.is_none());
                    fs::remove_file(&audio)?;
                    let absent = scan_paths(&[audio], &[], enabled)?;
                    assert!(absent.uncertainties.is_empty());
                    store.apply_scan(&absent)?;
                    assert!(store.track(id)?.unwrap().missing);
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn explicit_non_utf8_scopes_do_not_abort_valid_scan_reconciliation() -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    for force in [false, true] {
        for good_first in [false, true] {
            let directory = tempfile::tempdir()?;
            let root = directory.path().to_path_buf();
            let good_root = root.join("good");
            fs::create_dir(&good_root)?;
            let deleted = good_root.join("deleted.wav");
            wav(&deleted, 150)?;
            let store = crate::store::Store::open(Path::new(":memory:"))?;
            store.apply_scan(&scan_paths(std::slice::from_ref(&good_root), &[], false)?)?;
            let deleted_id = store
                .track_id_for_identity(crate::store::TrackIdentity::File(&deleted))?
                .unwrap();
            fs::remove_file(&deleted)?;
            let good = good_root.join("good.wav");
            wav(&good, 75)?;
            let bad_audio = root.join(OsStr::from_bytes(b"bad-\xff.wav"));
            let bad_directory = root.join(OsStr::from_bytes(b"bad-\xff"));
            let bad_sheet = root.join(OsStr::from_bytes(b"bad-\xff.cue"));
            let bad_absent = root.join(OsStr::from_bytes(b"absent-\xff.wav"));
            wav(&bad_audio, 75)?;
            fs::create_dir(&bad_directory)?;
            wav(&bad_directory.join("audio.wav"), 75)?;
            fs::write(&bad_sheet, b"invalid CUE")?;
            let invalid = [bad_audio.clone(), bad_directory, bad_sheet, bad_absent];
            let mut requested = invalid.to_vec();
            // Duplicate invalid scopes are normalized and reported only once.
            requested.push(root.join("unused/..").join(bad_audio.file_name().unwrap()));
            let normalized_good = good_root.join("unused/..");
            if good_first {
                requested.insert(0, normalized_good);
            } else {
                requested.push(normalized_good);
            }
            let mut events = Vec::new();
            let result = super::scan_paths(
                &requested,
                |paths| {
                    paths
                        .iter()
                        .map(|path| {
                            assert!(path.starts_with(&good_root));
                            if force {
                                Ok(None)
                            } else {
                                store.known_source(path)
                            }
                        })
                        .collect()
                },
                false,
                std::slice::from_ref(&root),
                1,
                &ScanCancellation::default(),
                |phase, processed, total, errors, path| {
                    events.push((phase, processed, total, errors, path.map(Path::to_path_buf)));
                },
            )?;
            assert_eq!(result.roots, [good_root]);
            assert_eq!(result.errors.len(), invalid.len());
            assert!(
                result
                    .errors
                    .iter()
                    .all(|error| error.contains("not UTF-8"))
            );
            assert!(result.uncertainties.is_empty());
            assert!(!result.scan_incomplete);
            assert_eq!(result.track_count(), 1);
            assert_eq!(result.records[0].path, good);
            for bad in &invalid {
                let reports: Vec<_> = events
                    .iter()
                    .filter(|event| event.4.as_ref() == Some(bad))
                    .collect();
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].0, ScanPhase::Discovering);
            }
            assert_eq!(
                events.last().unwrap(),
                &(ScanPhase::ReadingMetadata, 1, Some(1), invalid.len(), None)
            );
            assert!(events.windows(2).all(|pair| pair[0].3 <= pair[1].3));
            store.apply_scan(&result)?;
            assert!(store.track(deleted_id)?.unwrap().missing);
            let good_id = store
                .track_id_for_identity(crate::store::TrackIdentity::File(&good))?
                .unwrap();
            assert!(!store.track(good_id)?.unwrap().missing);
            store.remove_missing_tracks()?;
            assert_eq!(store.track_count()?, 1);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn invalid_only_scopes_finish_without_coverage_and_honor_cancellation() -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let good = root.join("good.wav");
    let bad = root.join(OsStr::from_bytes(b"bad-\xff.wav"));
    wav(&good, 75)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&good))?
        .unwrap();
    let mut events = Vec::new();
    let result = super::scan_paths(
        std::slice::from_ref(&bad),
        |_| panic!("invalid scope must not look up sources"),
        false,
        std::slice::from_ref(&root),
        1,
        &ScanCancellation::default(),
        |phase, processed, total, errors, path| {
            events.push((phase, processed, total, errors, path.map(Path::to_path_buf)));
        },
    )?;
    assert!(result.roots.is_empty());
    assert!(result.track_count() == 0);
    assert!(result.uncertainties.is_empty());
    assert_eq!(result.errors.len(), 1);
    assert_eq!(
        events.first().unwrap(),
        &(ScanPhase::Discovering, 0, None, 1, Some(bad.clone()))
    );
    assert_eq!(
        events.last().unwrap(),
        &(ScanPhase::ReadingMetadata, 0, Some(0), 1, None)
    );
    assert!(!store.apply_scan(&result)?.changed);
    assert!(!store.track(id)?.unwrap().missing);
    for requested in [vec![bad.clone()], vec![bad.clone(), good]] {
        let cancellation = ScanCancellation::default();
        let mut events = 0;
        let cancelled = super::scan_paths(
            &requested,
            |_| panic!("cancellation must prevent source lookups"),
            false,
            std::slice::from_ref(&root),
            1,
            &cancellation,
            |phase, processed, _, errors, path| {
                assert_eq!(phase, ScanPhase::Discovering);
                assert_eq!(processed, 0);
                assert_eq!(errors, 1);
                assert_eq!(path, Some(bad.as_path()));
                events += 1;
                cancellation.cancel();
            },
        );
        assert!(cancelled.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(events, 1);
    }
    Ok(())
}

#[test]
fn failed_audio_probe_is_not_missing_or_cleanable_but_true_deletion_is() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let audio = root.join("audio.wav");
    let deleted = root.join("deleted.wav");
    wav(&audio, 150)?;
    wav(&deleted, 150)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&audio))?
        .unwrap();
    let deleted_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&deleted))?
        .unwrap();
    store.set_favorite(&[id], true)?;
    store.edit_track(id, Some("Kept title"), None, None, None)?;
    fs::write(&audio, b"not audio")?;
    fs::remove_file(&deleted)?;
    let failed = scan_paths(std::slice::from_ref(&root), &[], false)?;
    assert_eq!(failed.uncertainties, [ScanUncertainty::File(audio.clone())]);
    store.apply_scan(&failed)?;
    assert!(!store.track(id)?.unwrap().missing);
    assert!(store.track(deleted_id)?.unwrap().missing);
    store.remove_missing_tracks()?;
    let preserved = store.track(id)?.unwrap();
    assert!(preserved.favorite);
    assert_eq!(preserved.title, "Kept title");
    assert!(store.track(deleted_id)?.is_none());
    fs::remove_file(&audio)?;
    let absent = scan_paths(std::slice::from_ref(&audio), &[], false)?;
    assert!(absent.uncertainties.is_empty());
    store.apply_scan(&absent)?;
    assert!(store.track(id)?.unwrap().missing);
    Ok(())
}

#[test]
fn discovered_file_not_found_is_uncertain_until_a_later_explicit_request() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let audio = root.join("audio.wav");
    wav(&audio, 150)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&audio))?
        .unwrap();
    let failed = scan_paths_at_depth(
        std::slice::from_ref(&root),
        &[],
        false,
        1,
        |phase, processed, _, _, _| {
            if phase == ScanPhase::ReadingMetadata && processed == 0 {
                fs::remove_file(&audio).expect("remove discovered audio before reading metadata");
            }
        },
    )?;
    assert_eq!(failed.uncertainties, [ScanUncertainty::File(audio.clone())]);
    store.apply_scan(&failed)?;
    assert!(!store.track(id)?.unwrap().missing);
    store.apply_scan(&scan_paths(&[audio], &[], false)?)?;
    assert!(store.track(id)?.unwrap().missing);
    Ok(())
}

#[test]
fn failed_cue_read_preserves_owned_tracks_and_independent_whole_audio() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let audio = root.join("audio.wav");
    let sheet = root.join("album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
    let first_id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))?
        .unwrap();
    let second_id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 2))?
        .unwrap();
    store.set_favorite(&[first_id], true)?;
    let failed = scan_paths_at_depth(
        std::slice::from_ref(&root),
        &[],
        false,
        1,
        |phase, processed, _, _, _| {
            if phase == ScanPhase::ReadingMetadata && processed == 0 {
                fs::remove_file(&sheet).expect("remove discovered CUE before reading it");
            }
        },
    )?;
    assert_eq!(
        failed.uncertainties,
        [ScanUncertainty::CueSheet(sheet.clone())]
    );
    assert!(failed.tracks().all(|track| !track.duration_filter_exempt));
    assert_eq!(failed.track_count(), 1);
    assert_eq!(failed.records[0].path, audio);
    store.apply_scan(&failed)?;
    store.remove_missing_tracks()?;
    assert!(store.track(first_id)?.unwrap().favorite);
    assert!(!store.track(second_id)?.unwrap().missing);
    let absent = scan_paths(&[sheet], &[], false)?;
    assert!(absent.uncertainties.is_empty());
    store.apply_scan(&absent)?;
    assert!(store.track(first_id)?.unwrap().missing);
    assert!(store.track(second_id)?.unwrap().missing);
    Ok(())
}

#[test]
#[cfg(unix)]
fn non_utf8_cue_name_does_not_prevent_valid_audio_from_being_saved() -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let audio = root.join("audio.wav");
    let unrelated = root.join("unrelated.wav");
    let sheet = root.join(OsStr::from_bytes(b"album-\xff.cue"));
    wav(&audio, 225)?;
    wav(&unrelated, 75)?;
    two_track_sheet(&sheet)?;

    let scan = scan_paths(std::slice::from_ref(&root), &[], false)?;
    assert_eq!(scan.uncertainties, [ScanUncertainty::CueSheet(sheet)]);
    assert_eq!(scan.errors.len(), 1);
    assert!(scan.tracks().all(|track| !track.duration_filter_exempt));
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan)?;
    assert_eq!(store.track_count()?, 2);
    for path in [audio, unrelated] {
        let id = store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))?
            .unwrap();
        assert!(!store.track(id)?.unwrap().missing);
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn forced_scan_isolates_non_utf8_audio_and_persists_valid_tracks() -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let good = root.join("good.wav");
    let bad = root.join(OsStr::from_bytes(b"bad-\xff.wav"));
    wav(&good, 75)?;
    wav(&bad, 75)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    // Force supplies no known-source snapshots, bypassing Store's path check.
    let forced = scan_paths(std::slice::from_ref(&root), &[], false)?;
    assert_eq!(forced.errors.len(), 1);
    assert!(forced.errors[0].contains("not UTF-8"));
    assert_eq!(forced.uncertainties, [ScanUncertainty::File(bad)]);
    assert_eq!(forced.track_count(), 1);
    assert_eq!(forced.records[0].path, good);
    store.apply_scan(&forced)?;
    assert_eq!(store.track_count()?, 1);
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&good))?
        .unwrap();
    assert!(!store.track(id)?.unwrap().missing);
    Ok(())
}

#[test]
fn failed_cue_source_not_found_keeps_segments_until_the_sheet_is_deleted() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let sheet = directory.path().join("album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&sheet), &[], false)?)?;
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))?
        .unwrap();
    fs::remove_file(&audio)?;
    let failed = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
    assert!(failed.track_count() == 0);
    assert!(
        failed
            .uncertainties
            .contains(&ScanUncertainty::CueSheet(sheet.clone()))
    );
    assert!(failed.uncertainties.contains(&ScanUncertainty::File(audio)));
    store.apply_scan(&failed)?;
    store.remove_missing_tracks()?;
    assert!(!store.track(id)?.unwrap().missing);
    fs::remove_file(&sheet)?;
    store.apply_scan(&scan_paths(&[sheet], &[], false)?)?;
    assert!(store.track(id)?.unwrap().missing);
    Ok(())
}

#[cfg(unix)]
#[test]
fn traversal_not_found_preserves_the_failed_subtree_and_its_identities() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let actual = tempfile::tempdir()?;
    let target = actual.path().join("target");
    fs::create_dir(&target)?;
    let root = directory.path().to_path_buf();
    let branch = root.join("branch");
    std::os::unix::fs::symlink(&target, &branch)?;
    let audio = branch.join("audio.wav");
    let sheet = branch.join("album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let deleted = root.join("deleted.wav");
    wav(&deleted, 150)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(&[root.clone(), audio.clone()], &[], false)?)?;
    let audio_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&audio))?
        .unwrap();
    let cue_id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))?
        .unwrap();
    let deleted_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&deleted))?
        .unwrap();
    store.set_favorite(&[audio_id, cue_id], true)?;
    let copy = root.join("copy.wav");
    fs::copy(&audio, &copy)?;
    fs::remove_dir_all(&target)?;
    fs::remove_file(deleted)?;
    let partial = scan_paths(std::slice::from_ref(&root), &[], false)?;
    assert_eq!(
        partial.uncertainties,
        [ScanUncertainty::Subtree {
            root_index: 0,
            path: branch
        }]
    );
    assert!(!partial.scan_incomplete);
    store.apply_scan(&partial)?;
    assert_ne!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&copy))?
            .unwrap(),
        audio_id
    );
    assert_eq!(store.track(audio_id)?.unwrap().path, audio);
    assert!(store.track(deleted_id)?.unwrap().missing);
    store.remove_missing_tracks()?;
    for id in [audio_id, cue_id] {
        let preserved = store.track(id)?.unwrap();
        assert!(!preserved.missing);
        assert!(preserved.favorite);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn failed_requested_root_metadata_is_uncertain_not_deleted() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("root");
    fs::create_dir(&root)?;
    let audio = root.join("audio.wav");
    wav(&audio, 150)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&audio))?
        .unwrap();
    fs::rename(&root, directory.path().join("moved"))?;
    std::os::unix::fs::symlink(&root, &root)?;
    let failed = scan_paths(std::slice::from_ref(&root), &[], false)?;
    assert_eq!(
        failed.uncertainties,
        [ScanUncertainty::Root { root_index: 0 }]
    );
    store.apply_scan(&failed)?;
    assert!(!store.track(id)?.unwrap().missing);
    fs::remove_file(&root)?;
    store.apply_scan(&scan_paths(&[root], &[], false)?)?;
    assert!(store.track(id)?.unwrap().missing);
    Ok(())
}

#[test]
fn scan_uncertainty_metadata_is_bounded_with_an_incomplete_fallback() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for index in 0..=MAX_RECORDED_SCAN_UNCERTAINTIES {
        fs::write(
            directory.path().join(format!("broken-{index}.wav")),
            b"not audio",
        )?;
    }
    let result = scan_paths(&[directory.path().to_path_buf()], &[], false)?;
    assert_eq!(result.uncertainties.len(), MAX_RECORDED_SCAN_UNCERTAINTIES);
    assert!(result.scan_incomplete);
    assert!(result.track_count() == 0);
    Ok(())
}

#[test]
fn logical_paths_normalize_missing_paths_and_parent_components() -> Result<()> {
    let cwd = std::env::current_dir()?;
    assert_eq!(
        logical_path(Path::new("missing/.././song.wav"))?,
        cwd.join("song.wav")
    );
    assert_eq!(
        logical_path(&cwd.join("missing/../song.wav"))?,
        cwd.join("song.wav")
    );
    assert_eq!(
        logical_path(Path::new("/../../song.wav"))?,
        PathBuf::from("/song.wav")
    );
    Ok(())
}

#[test]
fn missing_requests_keep_normalized_logical_roots_and_progress_paths() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let requested = directory.path().join("unused/../missing.wav");
    let expected = directory.path().join("missing.wav");
    let mut progress_paths = Vec::new();
    let result = super::scan_paths(
        &[requested],
        |paths| Ok(vec![None; paths.len()]),
        false,
        &[directory.path().to_path_buf()],
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, path| {
            if let Some(path) = path {
                progress_paths.push(path.to_path_buf());
            }
        },
    )?;
    assert_eq!(result.roots.as_slice(), std::slice::from_ref(&expected));
    assert_eq!(result.errors.len(), 1);
    assert_eq!(progress_paths, [expected]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn linked_root_keeps_alias_in_records_roots_and_progress() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let actual = tempfile::tempdir()?;
    wav(&actual.path().join("audio.wav"), 150)?;
    let alias = directory.path().join("linked");
    std::os::unix::fs::symlink(actual.path(), &alias)?;
    let mut progress_paths = Vec::new();
    let result = super::scan_paths(
        &[alias.join("unused/../.")],
        |paths| Ok(vec![None; paths.len()]),
        false,
        std::slice::from_ref(&alias),
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, path| {
            if let Some(path) = path {
                progress_paths.push(path.to_path_buf());
            }
        },
    )?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.roots.as_slice(), std::slice::from_ref(&alias));
    assert_eq!(result.track_count(), 1);
    assert_eq!(result.records[0].path, alias.join("audio.wav"));
    assert!(!progress_paths.is_empty());
    assert!(
        progress_paths
            .iter()
            .all(|path| path == &alias.join("audio.wav"))
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn linked_files_and_directories_outside_roots_keep_distinct_aliases() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let actual = tempfile::tempdir()?;
    let source = actual.path().join("audio.wav");
    wav(&source, 150)?;
    for name in ["first.wav", "second.wav"] {
        std::os::unix::fs::symlink(&source, directory.path().join(name))?;
    }
    for name in ["first", "second"] {
        std::os::unix::fs::symlink(actual.path(), directory.path().join(name))?;
    }
    let result = scan_paths(&[directory.path().to_path_buf()], &[], false)?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let mut expected: Vec<_> = [
        "first.wav",
        "second.wav",
        "first/audio.wav",
        "second/audio.wav",
    ]
    .into_iter()
    .map(|name| directory.path().join(name))
    .collect();
    expected.sort();
    assert_eq!(
        result
            .records
            .iter()
            .map(|record| record.path.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        result
            .records
            .iter()
            .all(|record| record.fingerprint == result.records[0].fingerprint)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn ancestor_link_cycle_reports_error_without_losing_noncyclic_media() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let nested = directory.path().join("nested");
    fs::create_dir(&nested)?;
    wav(&directory.path().join("audio.wav"), 150)?;
    wav(&nested.join("other.wav"), 150)?;
    let back = nested.join("back");
    std::os::unix::fs::symlink(directory.path(), &back)?;
    let result = scan_paths(&[directory.path().to_path_buf()], &[], false)?;
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].contains(&back.display().to_string()));
    assert_eq!(
        result
            .records
            .iter()
            .map(|record| record.path.clone())
            .collect::<Vec<_>>(),
        [directory.path().join("audio.wav"), nested.join("other.wav")]
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn cue_in_linked_tree_keeps_sheet_and_source_aliases_across_roots() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let actual = tempfile::tempdir()?;
    let other_actual = tempfile::tempdir()?;
    let root = directory.path().join("library");
    fs::create_dir(&root)?;
    let tree = root.join("linked");
    let other_root = directory.path().join("other-library");
    std::os::unix::fs::symlink(actual.path(), &tree)?;
    std::os::unix::fs::symlink(other_actual.path(), &other_root)?;
    wav(&tree.join("audio.wav"), 150)?;
    let remote = other_root.join("remote.wav");
    wav(&remote, 150)?;
    let sheet = tree.join("album.cue");
    fs::write(
        &sheet,
        format!(
            "FILE \"./unused/../audio.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nFILE \"{}\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n",
            other_root.join("unused/../remote.wav").display()
        ),
    )?;
    let result = super::scan_paths(
        std::slice::from_ref(&root),
        |paths| Ok(vec![None; paths.len()]),
        false,
        &[root.clone(), other_root],
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.roots, [root]);
    assert_eq!(result.track_count(), 4);
    let segments: Vec<_> = result
        .tracks()
        .filter(|track| track.cue.is_some())
        .collect();
    assert_eq!(segments[0].source.path, tree.join("audio.wav"));
    assert_eq!(segments[1].source.path, remote);
    assert!(
        segments
            .iter()
            .all(|record| *record.cue.as_ref().unwrap().sheet == sheet)
    );
    let mut whole_paths: Vec<_> = result
        .tracks()
        .filter(|track| track.cue.is_none())
        .map(|track| track.source.path.clone())
        .collect();
    whole_paths.sort();
    let mut expected = vec![tree.join("audio.wav"), remote];
    expected.sort();
    assert_eq!(whole_paths, expected);
    assert!(result.tracks().all(|track| track.duration_filter_exempt));
    Ok(())
}

#[test]
fn scan_progress_reports_deduplicated_audio_and_sheet_candidates() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let sheet = directory.path().join("album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let paths = [
        directory.path().to_path_buf(),
        sheet.clone(),
        directory.path().join("unused/../."),
    ];
    let library_roots = vec![logical_path(directory.path())?];
    let mut events = Vec::new();
    let result = super::scan_paths(
        &paths,
        |paths| Ok(vec![None; paths.len()]),
        false,
        &library_roots,
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |phase, processed, total, errors, path| {
            events.push((phase, processed, total, errors, path.map(Path::to_path_buf)));
        },
    )?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.track_count(), 3);
    assert!(
        events
            .iter()
            .filter_map(|event| event.4.as_ref())
            .all(|path| { path == &audio || path == &sheet })
    );
    let discovery: Vec<_> = events
        .iter()
        .filter(|(phase, ..)| matches!(phase, &ScanPhase::Discovering))
        .collect();
    assert_eq!(discovery.last().unwrap().1, 2);
    assert!(discovery.iter().all(|event| event.2.is_none()));
    let metadata: Vec<_> = events
        .iter()
        .filter(|(phase, ..)| matches!(phase, &ScanPhase::ReadingMetadata))
        .collect();
    assert_eq!(metadata.first().unwrap().1, 0);
    assert_eq!(metadata.first().unwrap().2, Some(2));
    assert_eq!(metadata.last().unwrap().1, 2);
    assert!(metadata.iter().all(|event| event.1 <= event.2.unwrap()));
    Ok(())
}

#[test]
fn scan_depth_keeps_allowed_files_and_reports_skipped_branches() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let start = outer.path().join("nested/start");
    fs::create_dir_all(&start)?;
    let allowed = start.join("allowed");
    let sibling = start.join("sibling");
    let skipped = allowed.join("skipped");
    let empty = start.join("empty");
    fs::create_dir(&allowed)?;
    fs::create_dir(&sibling)?;
    fs::create_dir(&empty)?;
    fs::create_dir(&skipped)?;
    let allowed_file = allowed.join("allowed.wav");
    let sibling_file = sibling.join("sibling.wav");
    let skipped_file = skipped.join("skipped.wav");
    wav(&allowed_file, 150)?;
    wav(&sibling_file, 150)?;
    wav(&skipped_file, 150)?;
    let mut events = Vec::new();
    let result = scan_paths_at_depth(
        std::slice::from_ref(&start),
        &[],
        false,
        1,
        |phase, processed, total, errors, path| {
            events.push((phase, processed, total, errors, path.map(Path::to_path_buf)));
        },
    )?;
    let mut records: Vec<_> = result
        .tracks()
        .map(|record| record.source.path.clone())
        .collect();
    records.sort();
    let mut expected = vec![allowed_file.clone(), sibling_file.clone()];
    expected.sort();
    assert_eq!(records, expected);
    assert_eq!(
        result.skipped_directories,
        [SkippedDirectory {
            root_index: 0,
            path: skipped.clone()
        }]
    );
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].contains(&skipped.display().to_string()));
    assert!(result.errors[0].contains("1"));
    assert!(events.iter().any(|(phase, _, _, errors, path)| {
        matches!(phase, ScanPhase::Discovering)
            && *errors == 1
            && path.as_deref() == Some(skipped.as_path())
    }));
    let direct = scan_paths_at_depth(
        std::slice::from_ref(&skipped_file),
        &[],
        false,
        1,
        |_, _, _, _, _| {},
    )?;
    assert_eq!(direct.track_count(), 1);
    assert_eq!(direct.records[0].path, skipped_file);
    assert!(direct.skipped_directories.is_empty());
    Ok(())
}

#[test]
fn nested_scan_keeps_its_own_excluded_identity_and_user_data() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let nested = root.join("one/nested");
    let deep = nested.join("below/deeper/deep.wav");
    fs::create_dir_all(deep.parent().unwrap())?;
    wav(&deep, 150)?;
    let baseline = scan_paths_at_depth(
        std::slice::from_ref(&root),
        &[],
        false,
        5,
        |_, _, _, _, _| {},
    )?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&baseline)?;
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&deep))?
        .unwrap();
    store.set_favorite(&[id], true)?;
    store.edit_track(id, Some("Kept title"), None, None, None)?;
    let copy = nested.join("copy.wav");
    fs::rename(&deep, &copy)?;
    let partial = scan_paths_at_depth(&[root, nested], &[], false, 1, |_, _, _, _, _| {})?;
    store.apply_scan(&partial)?;
    let preserved = store.track(id)?.unwrap();
    assert_eq!(preserved.path, deep);
    assert!(!preserved.missing);
    assert!(preserved.favorite);
    assert_eq!(preserved.title, "Kept title");
    assert_ne!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&copy))?
            .unwrap(),
        id
    );
    Ok(())
}

#[test]
fn duplicate_normalized_requests_do_not_exhaust_scan_diagnostics() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::create_dir_all(directory.path().join("child/deeper"))?;
    let root = directory.path().to_path_buf();
    let paths = vec![root.join("unused/../."); MAX_REPORTED_SCAN_ERRORS + 1];
    let result = scan_paths_at_depth(&paths, &[], false, 1, |_, _, _, _, _| {})?;
    assert_eq!(result.errors.len(), 1);
    assert!(!result.scan_incomplete);
    assert!(!result.skipped_directories_truncated);
    Ok(())
}

#[test]
fn scan_depth_zero_is_rejected_by_library_api() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = logical_path(directory.path())?;
    let error = super::scan_paths(
        std::slice::from_ref(&root),
        |paths| Ok(vec![None; paths.len()]),
        false,
        std::slice::from_ref(&root),
        0,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )
    .expect_err("zero scan depth must be rejected");

    assert!(error.to_string().contains("at least 1"));
    Ok(())
}
#[test]
fn scan_error_and_skipped_directory_details_are_bounded() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let root = outer.path().join("root");
    fs::create_dir(&root)?;
    let missing: Vec<_> = (0..300)
        .map(|index| root.join(format!("missing-{index}.wav")))
        .collect();
    let errors = scan_paths_at_depth(&missing, &[], false, 1, |_, _, _, _, _| {})?;
    assert_eq!(errors.errors.len(), MAX_REPORTED_SCAN_ERRORS);
    assert!(errors.scan_incomplete);
    assert!(
        errors
            .errors
            .last()
            .unwrap()
            .contains("additional scan errors omitted")
    );

    for index in 0..300 {
        let branch = root.join(format!("branch-{index}")).join("too-deep");
        fs::create_dir_all(branch)?;
    }
    let skipped = scan_paths_at_depth(
        std::slice::from_ref(&root),
        &[],
        false,
        1,
        |_, _, _, _, _| {},
    )?;
    assert!(skipped.scan_incomplete);
    assert!(skipped.skipped_directories_truncated);
    assert!(skipped.skipped_directories.len() <= MAX_RECORDED_SKIPPED_DIRECTORIES);
    Ok(())
}

#[cfg(unix)]
#[test]
fn scan_depth_applies_to_logical_symlink_paths() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let actual = tempfile::tempdir()?;
    let start = directory.path().join("start");
    let link = start.join("linked");
    fs::create_dir(&start)?;
    std::os::unix::fs::symlink(actual.path(), &link)?;
    let audio = link.join("audio.wav");
    wav(&audio, 150)?;
    let deeper = link.join("deeper");
    fs::create_dir(&deeper)?;
    wav(&deeper.join("too-deep.wav"), 150)?;
    let result = scan_paths_at_depth(
        std::slice::from_ref(&start),
        &[],
        false,
        1,
        |_, _, _, _, _| {},
    )?;
    assert_eq!(
        result
            .records
            .iter()
            .map(|record| &record.path)
            .collect::<Vec<_>>(),
        vec![&audio]
    );
    assert_eq!(
        result.skipped_directories,
        [SkippedDirectory {
            root_index: 0,
            path: deeper
        }]
    );
    assert_eq!(result.errors.len(), 1);
    Ok(())
}

#[test]
fn cue_sources_require_logical_membership_in_any_configured_root() -> Result<()> {
    let inside = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let source = outside.path().join("outside.wav");
    let sheet = inside.path().join("outside.cue");
    wav(&source, 150)?;
    fs::write(
        &sheet,
        format!(
            "FILE \"{}\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
            source.display()
        ),
    )?;
    let library_roots = vec![logical_path(inside.path())?];
    let control = super::scan_paths(
        std::slice::from_ref(&sheet),
        |paths| Ok(vec![None; paths.len()]),
        false,
        &[logical_path(inside.path())?, logical_path(outside.path())?],
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )?;
    assert!(control.errors.is_empty(), "{:?}", control.errors);
    assert_eq!(
        control.tracks().next().unwrap().source.path,
        logical_path(&source)?
    );
    assert_eq!(
        control.tracks().filter(|track| track.cue.is_some()).count(),
        1
    );
    let mut events = Vec::new();
    let result = super::scan_paths(
        std::slice::from_ref(&sheet),
        |paths| Ok(vec![None; paths.len()]),
        false,
        &library_roots,
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |phase, processed, total, errors, path| {
            events.push((phase, processed, total, errors, path.map(Path::to_path_buf)));
        },
    )?;
    assert!(result.track_count() == 0);
    assert_eq!(result.errors.len(), 1);
    assert!(result.tracks().all(|track| !track.duration_filter_exempt));
    let metadata = events
        .iter()
        .rfind(|(phase, ..)| matches!(phase, &ScanPhase::ReadingMetadata))
        .unwrap();
    assert_eq!(metadata.1, 1);
    assert_eq!(metadata.2, Some(1));
    assert_eq!(metadata.3, 1);
    Ok(())
}

fn known_records(result: &ScanResult) -> Vec<(PathBuf, KnownFile)> {
    result
        .tracks()
        .map(|record| {
            let mut media = record.media.source.clone();
            media.title = record.media.title.to_owned();
            media.artist = record.media.artist.to_owned();
            media.album = record.media.album.to_owned();
            media.duration = record.media.duration;
            media.track_number = record.media.track_number;
            (
                record.source.path.clone(),
                KnownFile {
                    size: record.source.size,
                    modified_ns: record.source.modified_ns,
                    fingerprint: record.source.fingerprint.clone(),
                    media: Some(media),
                    cue: record.cue.map(cue_segment),
                },
            )
        })
        .collect()
}

fn cue_segment(cue: CueView<'_>) -> CueSegment {
    CueSegment {
        sheet: cue.sheet.clone(),
        number: cue.number,
        start_frame: cue.start_frame,
        end_frame: cue.end_frame,
    }
}

#[test]
fn signed_mtime_cache_keys_preserve_epoch_and_reject_unavailable_or_out_of_range() {
    let maximum = i64::MAX as u64;
    for (modified, expected) in [
        (None, UNCACHEABLE_MODIFIED_NS),
        (Some(UNIX_EPOCH), 0),
        (Some(UNIX_EPOCH - Duration::from_nanos(1)), -1),
        (Some(UNIX_EPOCH + Duration::from_nanos(maximum)), i64::MAX),
        (Some(UNIX_EPOCH - Duration::from_nanos(maximum)), -i64::MAX),
        (
            Some(UNIX_EPOCH + Duration::from_nanos(maximum + 1)),
            UNCACHEABLE_MODIFIED_NS,
        ),
        (
            Some(UNIX_EPOCH - Duration::from_nanos(maximum + 1)),
            UNCACHEABLE_MODIFIED_NS,
        ),
        (
            Some(UNIX_EPOCH - Duration::from_nanos(maximum + 2)),
            UNCACHEABLE_MODIFIED_NS,
        ),
    ] {
        let modified_ns = cache_modified_ns(modified);
        assert_eq!(modified_ns, expected);
        let known = KnownFile {
            size: 1,
            modified_ns,
            fingerprint: None,
            media: None,
            cue: None,
        };
        assert_eq!(
            known.matches_metadata(1, modified_ns),
            modified_ns != UNCACHEABLE_MODIFIED_NS
        );
        assert!(!known.matches_metadata(2, modified_ns));
    }
}

#[test]
fn ordinary_scan_rehashes_same_size_wav_after_pre_epoch_mtime_change() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    wav(&audio, 150)?;
    File::options()
        .write(true)
        .open(&audio)?
        .set_modified(UNIX_EPOCH - Duration::from_secs(2))?;
    let original = scan_paths(std::slice::from_ref(&audio), &[], false)?;
    assert!(original.errors.is_empty(), "{:?}", original.errors);
    assert_eq!(original.records[0].modified_ns, -2_000_000_000);
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&original)?;
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&audio))?
        .unwrap();
    let known = vec![(audio.clone(), store.known_source(&audio)?.unwrap())];
    assert_eq!(known[0].1.modified_ns, -2_000_000_000);

    let mut contents = fs::read(&audio)?;
    *contents.last_mut().unwrap() = 1;
    fs::write(&audio, contents)?;
    File::options()
        .write(true)
        .open(&audio)?
        .set_modified(UNIX_EPOCH - Duration::from_secs(1))?;
    let rescanned = scan_paths(std::slice::from_ref(&audio), &known, false)?;
    assert!(rescanned.errors.is_empty(), "{:?}", rescanned.errors);
    assert_eq!(rescanned.records[0].size, original.records[0].size);
    assert_eq!(rescanned.records[0].modified_ns, -1_000_000_000);
    assert_ne!(
        rescanned.records[0].fingerprint,
        original.records[0].fingerprint
    );
    store.apply_scan(&rescanned)?;
    assert_eq!(
        store.track_id_for_identity(crate::store::TrackIdentity::File(&audio))?,
        Some(id)
    );
    assert_eq!(
        store.track(id)?.unwrap().fingerprint,
        rescanned.records[0].fingerprint
    );
    Ok(())
}

#[test]
fn epoch_zero_reuses_cache_while_forced_scan_rehashes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    wav(&audio, 150)?;
    File::options()
        .write(true)
        .open(&audio)?
        .set_modified(UNIX_EPOCH)?;
    let original = scan_paths(std::slice::from_ref(&audio), &[], false)?;
    assert!(original.errors.is_empty(), "{:?}", original.errors);
    assert_eq!(original.records[0].modified_ns, 0);
    let known = known_records(&original);

    // Preserve size and the genuine epoch timestamp to exercise ordinary reuse.
    let mut contents = fs::read(&audio)?;
    *contents.last_mut().unwrap() = 1;
    fs::write(&audio, contents)?;
    File::options()
        .write(true)
        .open(&audio)?
        .set_modified(UNIX_EPOCH)?;
    let ordinary = scan_paths(std::slice::from_ref(&audio), &known, false)?;
    assert!(ordinary.errors.is_empty(), "{:?}", ordinary.errors);
    assert_eq!(ordinary.records[0].modified_ns, 0);
    assert_eq!(
        ordinary.records[0].fingerprint,
        original.records[0].fingerprint
    );

    // The owner implements force by supplying no reusable source snapshots.
    let forced = scan_paths(std::slice::from_ref(&audio), &[], false)?;
    assert!(forced.errors.is_empty(), "{:?}", forced.errors);
    assert_eq!(forced.records[0].size, original.records[0].size);
    assert_eq!(forced.records[0].modified_ns, 0);
    assert_ne!(
        forced.records[0].fingerprint,
        original.records[0].fingerprint
    );
    Ok(())
}

fn track_from_record(id: i64, record: ScanTrack<'_>) -> Track {
    Track {
        id,
        path: record.source.path.clone(),
        fingerprint: record.source.fingerprint.clone(),
        title: record.media.title.to_owned(),
        artist: record.media.artist.to_owned(),
        album: record.media.album.to_owned(),
        duration: record.media.duration,
        bitrate_bps: record.media.source.bitrate_bps,
        bitrate_exact: record.media.source.bitrate_exact,
        track_number: record.media.track_number,
        disc_number: record.media.disc_number,
        bits_per_sample: record.media.source.bits_per_sample,
        release_date: record.media.release_date.map(str::to_owned),
        favorite: false,
        decoder_hint: Default::default(),
        codec: record.media.source.codec.clone(),
        channels: record.media.source.channels,
        sample_rate: record.media.source.sample_rate,
        cue: record.cue.map(cue_segment),
        missing: false,
        play_count: 0,
        last_played: None,
    }
}

#[test]
fn cue_single_source_tracks_have_relative_duration_and_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let sheet = directory.path().join("album.CUE");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let result = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.track_count(), 3);
    let first = &result
        .tracks()
        .find(|track| track.cue.is_some_and(|cue| cue.number == 1))
        .unwrap();
    let second = &result
        .tracks()
        .find(|track| track.cue.is_some_and(|cue| cue.number == 2))
        .unwrap();
    assert_eq!(first.source.path, logical_path(&audio)?);
    assert_eq!(second.source.path, first.source.path);
    assert_eq!(first.media.title, "First");
    assert_eq!(first.media.artist, "Track artist");
    assert_eq!(second.media.title, "Track 02");
    assert_eq!(second.media.artist, "Sheet artist");
    assert_eq!(first.media.album, "Album");
    assert_eq!(first.media.duration, Some(1.0));
    assert_eq!(second.media.duration, Some(2.0));
    assert_eq!(first.media.source.bitrate_bps, Some(120_000));
    assert_eq!(
        second.media.source.bitrate_bps,
        first.media.source.bitrate_bps
    );
    assert_eq!(first.media.source.bits_per_sample, Some(16));
    assert_eq!(first.media.track_number, Some(1));
    assert_eq!(second.media.track_number, Some(2));
    assert_eq!(
        cue_segment(first.cue.unwrap()),
        CueSegment {
            sheet: logical_path(&sheet)?,
            number: 1,
            start_frame: 0,
            end_frame: Some(75),
        }
    );
    assert_eq!(second.cue.as_ref().unwrap().start_frame, 75);
    assert_eq!(second.cue.as_ref().unwrap().end_frame, None);
    Ok(())
}

#[test]
fn cue_multiple_files_reset_indexes_and_keep_sheet_order() -> Result<()> {
    let directory = tempfile::tempdir()?;
    wav(&directory.path().join("z.wav"), 150)?;
    wav(&directory.path().join("a.wav"), 225)?;
    let sheet = directory.path().join("multi.cue");
    fs::write(
        &sheet,
        "FILE \"z.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00\nFILE \"a.wav\" WAVE\nTRACK 03 AUDIO\nINDEX 01 00:00:00\nTRACK 04 AUDIO\nINDEX 01 00:02:00\n",
    )?;
    let result = scan_paths(&[sheet], &[], false)?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.track_count(), 6);
    assert_eq!(
        result
            .tracks()
            .filter(|track| track.cue.is_some())
            .map(|record| record.cue.as_ref().unwrap().number)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        result
            .tracks()
            .filter(|track| track.cue.is_some())
            .map(|record| record.media.duration)
            .collect::<Vec<_>>(),
        [Some(1.0), Some(1.0), Some(2.0), Some(1.0)]
    );
    assert_eq!(
        result
            .tracks()
            .filter(|track| track.cue.is_some())
            .nth(1)
            .unwrap()
            .cue
            .as_ref()
            .unwrap()
            .end_frame,
        None
    );
    assert_eq!(
        result
            .tracks()
            .filter(|track| track.cue.is_some())
            .nth(2)
            .unwrap()
            .cue
            .as_ref()
            .unwrap()
            .start_frame,
        0
    );
    assert_eq!(
        result.tracks().filter(|track| track.cue.is_none()).count(),
        2
    );
    Ok(())
}

#[test]
fn cue_rescan_rereads_sheet_and_never_reuses_segment_as_full_media() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let sheet = directory.path().join("album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let original = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
    assert!(original.errors.is_empty(), "{:?}", original.errors);
    let mut known: Vec<_> = known_records(&original)
        .into_iter()
        .filter(|(_, known)| known.cue.is_some())
        .collect();
    for (_, known) in &mut known {
        known.fingerprint = Some("unchanged-cue-source-hash".into());
    }
    let full = scan_paths(std::slice::from_ref(&audio), &known, false)?;
    assert!(full.errors.is_empty(), "{:?}", full.errors);
    assert_eq!(full.track_count(), 1);
    assert_eq!(full.records[0].media.duration, Some(3.0));
    assert_ne!(full.records[0].media.artist, "Sheet artist");
    assert_eq!(
        full.records[0].fingerprint.as_deref(),
        Some("unchanged-cue-source-hash")
    );
    fs::write(
        &sheet,
        "TITLE \"Edited album\"\nFILE \"audio.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"Renamed\"\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:02:00\n",
    )?;
    for cache in [&known, &known_records(&full)] {
        let rescanned = scan_paths(std::slice::from_ref(&sheet), cache, false)?;
        assert!(rescanned.errors.is_empty(), "{:?}", rescanned.errors);
        assert_eq!(rescanned.track_count(), 3);
        let first = rescanned
            .tracks()
            .find(|track| track.cue.is_some_and(|cue| cue.number == 1))
            .unwrap();
        let second = rescanned
            .tracks()
            .find(|track| track.cue.is_some_and(|cue| cue.number == 2))
            .unwrap();
        assert_eq!(first.media.title, "Renamed");
        assert_eq!(first.media.album, "Edited album");
        assert_eq!(first.media.artist, full.records[0].media.artist);
        assert_eq!(first.media.duration, Some(2.0));
        assert_eq!(second.media.duration, Some(1.0));
        assert_eq!(
            rescanned.tracks().next().unwrap().source.fingerprint,
            known[0].1.fingerprint
        );
    }
    Ok(())
}

#[test]
fn cue_missing_source_and_out_of_bounds_fail_atomically() -> Result<()> {
    let directory = tempfile::tempdir()?;
    wav(&directory.path().join("audio.wav"), 150)?;
    let sheet = directory.path().join("broken.cue");
    for invalid in [
        "FILE \"missing.wav\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n",
        "TRACK 02 AUDIO\nINDEX 01 00:02:00\n",
        "TRACK 02 AUDIO\nINDEX 01 00:02:02\n",
        "TRACK 02 AUDIO\nINDEX 01 00:00:00\n",
    ] {
        fs::write(
            &sheet,
            format!("FILE \"audio.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n{invalid}"),
        )?;
        let result = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
        assert!(result.track_count() == 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.tracks().all(|track| !track.duration_filter_exempt));
        let directory_scan = scan_paths(&[directory.path().to_path_buf()], &[], false)?;
        assert_eq!(directory_scan.track_count(), 1);
        assert!(
            directory_scan
                .tracks()
                .all(|track| !track.duration_filter_exempt)
        );
        let mut filtered = directory_scan;
        filtered.min_duration_secs = 10;
        let store = crate::store::Store::open(Path::new(":memory:"))?;
        assert_eq!(store.apply_scan(&filtered)?.skipped_short_tracks, 1);
        assert_eq!(store.track_count()?, 0);
    }
    Ok(())
}

#[test]
fn cue_and_directory_requests_keep_visible_sources_without_duplicates() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let sheet = directory.path().join("album.cue");
    wav(&audio, 225)?;
    wav(&directory.path().join("unrelated.wav"), 75)?;
    two_track_sheet(&sheet)?;
    let result = scan_paths(&[directory.path().to_path_buf(), sheet.clone()], &[], false)?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.track_count(), 4);
    assert_eq!(
        result
            .tracks()
            .filter(|record| record.cue.is_none())
            .count(),
        2
    );
    assert_eq!(
        result
            .tracks()
            .filter(|track| track.duration_filter_exempt)
            .count(),
        3
    );
    let explicit = scan_paths(
        &[directory.path().to_path_buf(), audio.clone(), sheet, audio],
        &[],
        false,
    )?;
    assert!(explicit.errors.is_empty(), "{:?}", explicit.errors);
    assert_eq!(explicit.track_count(), 4);
    assert_eq!(
        explicit
            .tracks()
            .filter(|record| record.cue.is_none())
            .count(),
        2
    );
    assert_eq!(
        explicit
            .tracks()
            .filter(|track| track.duration_filter_exempt)
            .count(),
        3
    );
    Ok(())
}

#[test]
fn cue_sheets_share_source_snapshot_with_explicit_full_audio() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let first_sheet = directory.path().join("a.cue");
    let second_sheet = directory.path().join("b.cue");
    wav(&audio, 225)?;
    two_track_sheet(&first_sheet)?;
    fs::write(
        &second_sheet,
        "TITLE \"Other album\"\nFILE \"audio.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"Other track\"\nINDEX 01 00:01:00\n",
    )?;
    let original = scan_paths(std::slice::from_ref(&audio), &[], false)?;
    assert!(original.errors.is_empty(), "{:?}", original.errors);
    let mut removed_source = false;
    let result = scan_paths_at_depth(
        &[directory.path().to_path_buf(), audio.clone(), audio.clone()],
        &[],
        false,
        crate::config::Config::default().scan_max_depth,
        |phase, _, _, _, path| {
            if phase == ScanPhase::ReadingMetadata && path == Some(first_sheet.as_path()) {
                // Later sheets and the full-file request must use this scan's snapshot.
                fs::remove_file(&audio).expect("remove source after first CUE sheet");
                removed_source = true;
            }
        },
    )?;
    assert!(removed_source);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.track_count(), 4);
    assert!(result.tracks().all(|track| track.duration_filter_exempt));
    let full = result.tracks().find(|record| record.cue.is_none()).unwrap();
    assert_eq!(full.source.path, audio);
    assert_eq!(full.media.duration, Some(3.0));
    assert_eq!(full.media.title, original.records[0].media.title);
    assert_eq!(full.source.fingerprint, original.records[0].fingerprint);
    let segments: Vec<_> = result
        .tracks()
        .filter(|record| record.cue.is_some())
        .collect();
    assert_eq!(*segments[0].cue.as_ref().unwrap().sheet, first_sheet);
    assert_eq!(segments[0].media.duration, Some(1.0));
    assert_eq!(segments[1].media.duration, Some(2.0));
    assert_eq!(*segments[2].cue.as_ref().unwrap().sheet, second_sheet);
    assert_eq!(segments[2].media.title, "Other track");
    assert_eq!(segments[2].media.album, "Other album");
    assert_eq!(segments[2].media.duration, Some(2.0));
    assert!(
        segments
            .iter()
            .all(|record| record.source.fingerprint == full.source.fingerprint)
    );
    Ok(())
}

#[test]
fn cue_source_errors_are_reused_for_later_sheets_and_ordinary_audio() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let first_sheet = directory.path().join("a.cue");
    let second_sheet = directory.path().join("b.cue");
    fs::write(&audio, b"invalid audio")?;
    two_track_sheet(&first_sheet)?;
    two_track_sheet(&second_sheet)?;
    let mut repaired_source = false;
    let result = scan_paths_at_depth(
        &[directory.path().to_path_buf(), audio.clone()],
        &[],
        false,
        crate::config::Config::default().scan_max_depth,
        |phase, _, _, _, path| {
            if phase == ScanPhase::ReadingMetadata && path == Some(first_sheet.as_path()) {
                // A repaired source must not cause a second probe in this scan.
                wav(&audio, 225).expect("repair source after first CUE sheet");
                repaired_source = true;
            }
        },
    )?;
    assert!(repaired_source);
    assert!(result.track_count() == 0);
    assert!(result.tracks().all(|track| !track.duration_filter_exempt));
    assert_eq!(result.errors.len(), 3);
    let details: Vec<_> = result
        .errors
        .iter()
        .zip([&first_sheet, &second_sheet, &audio])
        .map(|(error, path)| {
            error
                .strip_prefix(&format!("{}: ", path.display()))
                .expect("scan error must retain its candidate path")
        })
        .collect();
    assert!(!details[0].is_empty());
    assert_eq!(details[1], details[0]);
    assert_eq!(details[2], details[0]);
    Ok(())
}

fn export_track(track: &Track) -> PlaylistExportTrack {
    PlaylistExportTrack {
        path: track.path.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        duration: track.duration,
        cue: track.cue.clone(),
    }
}

fn playlist_tracks<'a>(
    track_ids: &'a [i64],
    tracks: &'a [Track],
) -> impl Iterator<Item = Result<PlaylistExportTrack>> + 'a {
    track_ids.iter().map(|id| {
        tracks
            .iter()
            .find(|track| track.id == *id)
            .map(export_track)
            .context("Missing fixture track")
    })
}

// Scan/import fixtures stay synchronous; drive the actual writer's awaited
// filesystem operations and streaming input boundary with the futures executor.
fn export_m3u<I>(path: &Path, tracks: I, cancellation: &ScanCancellation) -> Result<()>
where
    I: IntoIterator<Item = Result<PlaylistExportTrack>>,
{
    let workers = &crate::execution::BlockingExecutor::new(4);
    futures::executor::block_on(super::export_m3u(
        workers,
        path,
        futures::stream::iter(tracks),
        cancellation,
    ))
}

#[test]
#[cfg(unix)]
fn logical_cue_alias_paths_roundtrip_complete_sheets() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let actual = tempfile::tempdir()?;
    let alias = directory.path().join("linked");
    std::os::unix::fs::symlink(actual.path(), &alias)?;
    let audio = alias.join("audio.wav");
    let sheet = alias.join("#album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let scan = scan_paths(&[sheet.clone(), audio], &[], false)?;
    assert!(scan.errors.is_empty(), "{:?}", scan.errors);
    assert!(
        scan.tracks()
            .all(|record| record.source.path == alias.join("audio.wav"))
    );
    let tracks: Vec<_> = scan
        .tracks()
        .enumerate()
        .map(|(index, record)| track_from_record(index as i64 + 1, record))
        .collect();
    let mut sequence: Vec<_> = tracks
        .iter()
        .filter(|track| track.cue.is_some())
        .map(|track| track.id)
        .collect();
    sequence.extend_from_within(..);
    sequence.push(tracks.iter().find(|track| track.cue.is_none()).unwrap().id);
    sequence.push(*sequence.last().unwrap());
    let path = alias.join("playlist.m3u8");
    export_m3u(
        &path,
        playlist_tracks(&sequence, &tracks),
        &ScanCancellation::default(),
    )?;
    let imported = import_playlist(&path)?;
    let expected = sequence;
    assert_eq!(imported.len(), expected.len());
    for (item, id) in imported.iter().zip(expected) {
        let track = tracks.iter().find(|track| track.id == id).unwrap();
        assert_eq!(item.cue_track, track.cue.as_ref().map(|cue| cue.number));
        assert_eq!(
            item.path,
            *track.cue.as_ref().map_or(&track.path, |cue| &cue.sheet)
        );
    }
    // Ordinary path entries, usable without interpreting player-specific tags.
    let text = fs::read_to_string(&path)?;
    assert_eq!(
        text.lines().filter(|line| *line == "./#album.cue").count(),
        2
    );
    Ok(())
}

#[test]
fn unrepresentable_cue_exports_preserve_existing_destination() -> Result<()> {
    let directory = tempfile::tempdir()?;
    wav(&directory.path().join("audio.wav"), 225)?;
    let sheet = directory.path().join("album.cue");
    two_track_sheet(&sheet)?;
    let scan = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
    let tracks: Vec<_> = scan
        .tracks()
        .filter(|track| track.cue.is_some())
        .enumerate()
        .map(|(index, record)| track_from_record(index as i64 + 1, record))
        .collect();
    let path = directory.path().join("existing.m3u8");
    fs::write(&path, "original playlist\n")?;
    for sequence in [vec![1], vec![2], vec![2, 1]] {
        assert!(
            export_m3u(
                &path,
                playlist_tracks(&sequence, &tracks),
                &ScanCancellation::default()
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path)?, "original playlist\n");
    }
    // A sheet changed outside Rivu must not silently change exported ranges.
    let stale_sequence = [1, 2];
    fs::write(
        &sheet,
        fs::read_to_string(&sheet)?.replace("00:01:00", "00:02:00"),
    )?;
    assert!(
        export_m3u(
            &path,
            playlist_tracks(&stale_sequence, &tracks),
            &ScanCancellation::default()
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(&path)?, "original playlist\n");
    Ok(())
}

#[test]
#[cfg(unix)]
fn cue_export_rejects_fifo_and_oversized_sheets_without_replacing_destination() -> Result<()> {
    let directory = tempfile::tempdir()?;
    wav(&directory.path().join("audio.wav"), 225)?;
    let sheet = directory.path().join("album.cue");
    two_track_sheet(&sheet)?;
    let scan = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
    let tracks: Vec<_> = scan
        .tracks()
        .filter(|track| track.cue.is_some())
        .enumerate()
        .map(|(index, record)| track_from_record(index as i64 + 1, record))
        .collect();
    let destination = directory.path().join("existing.m3u8");
    let sequence = [1, 2];
    export_m3u(
        &destination,
        playlist_tracks(&sequence, &tracks),
        &ScanCancellation::default(),
    )?;
    assert_eq!(import_playlist(&destination)?.len(), 2);
    let original = fs::read(&destination)?;

    fs::remove_file(&sheet)?;
    let name = std::ffi::CString::new(sheet.as_os_str().as_encoded_bytes())?;
    // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (completed, receive) = std::sync::mpsc::channel();
    let export_path = destination.clone();
    let worker = std::thread::spawn(move || {
        let result = export_m3u(
            &export_path,
            playlist_tracks(&sequence, &tracks),
            &ScanCancellation::default(),
        );
        completed.send(result).unwrap();
    });
    // Do not join on timeout: a regression must fail rather than hang the suite.
    let error = receive.recv_timeout(Duration::from_secs(2))?.unwrap_err();
    worker.join().unwrap();
    assert!(format!("{error:#}").contains("not an ordinary file"));
    assert_eq!(fs::read(&destination)?, original);

    fs::remove_file(&sheet)?;
    two_track_sheet(&sheet)?;
    File::options()
        .write(true)
        .open(&sheet)?
        .set_len(1024 * 1024 + 1)?;
    let tracks = scan
        .tracks()
        .filter(|track| track.cue.is_some())
        .enumerate()
        .map(|(index, record)| Ok(export_track(&track_from_record(index as i64 + 1, record))));
    assert!(export_m3u(&destination, tracks, &ScanCancellation::default()).is_err());
    assert_eq!(fs::read(&destination)?, original);
    Ok(())
}

fn assert_playlist_roundtrip(path: &Path) -> Result<()> {
    let directory = logical_path(path.parent().unwrap())?;
    let tracks: Vec<_> = ["#song.wav", "other song.wav"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| Track {
            id: index as i64 + 1,
            path: directory.join(name),
            fingerprint: None,
            title: name.into(),
            artist: if index == 0 {
                "Artist".into()
            } else {
                String::new()
            },
            album: String::new(),
            duration: Some(2.0),
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
            decoder_hint: Default::default(),
            codec: "wav".into(),
            channels: 2,
            sample_rate: 48_000,
            missing: false,
            play_count: 0,
            last_played: None,
            cue: None,
        })
        .collect();
    for track in &tracks {
        fs::write(&track.path, b"playlist path fixture")?;
    }
    let sequence = [2, 1];
    export_m3u(
        path,
        playlist_tracks(&sequence, &tracks),
        &ScanCancellation::default(),
    )?;
    let text = fs::read_to_string(logical_path(path)?)?;
    assert_eq!(
        text.lines().filter(|line| *line == "./#song.wav").count(),
        1
    );
    let imported = import_m3u(path)?;
    assert_eq!(imported.len(), 2);
    for (item, index) in imported.iter().zip([1, 0]) {
        assert_eq!(item.path, tracks[index].path);
        assert_eq!(item.cue_track, None);
    }
    Ok(())
}

#[test]
fn m3u_roundtrip_preserves_hash_paths_and_first_track_order() -> Result<()> {
    let directory = tempfile::tempdir()?;
    assert_playlist_roundtrip(&directory.path().join("playlist.m3u8"))
}

#[test]
fn m3u_metadata_line_breaks_preserve_imported_path_sequence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let paths: Vec<_> = ["#first.wav", "second.wav", "third.wav"]
        .into_iter()
        .map(|name| directory.path().join(name))
        .collect();
    for path in &paths {
        wav(path, 75)?;
    }
    let scan = scan_paths(&paths, &[], false)?;
    assert!(scan.errors.is_empty(), "{:?}", scan.errors);
    assert_eq!(scan.track_count(), 3);
    let mut tracks: Vec<_> = scan
        .tracks()
        .enumerate()
        .map(|(index, record)| track_from_record(index as i64 + 1, record))
        .collect();
    tracks[0].artist = "Artist\r\nextra.wav".into();
    tracks[0].title = "Title\n#EXTINF:9,Injected\rTail".into();
    tracks[1].artist.clear();
    tracks[1].title = "Title only\r\nextra.wav".into();
    tracks[2].artist = "Artist only\nextra.wav\rTail".into();
    tracks[2].title.clear();
    let sequence = [3, 1, 2];
    let destination = directory.path().join("playlist.m3u8");
    export_m3u(
        &destination,
        playlist_tracks(&sequence, &tracks),
        &ScanCancellation::default(),
    )?;
    let imported = import_m3u(&destination)?;
    assert_eq!(imported.len(), sequence.len());
    for (item, id) in imported.iter().zip(sequence) {
        let expected = tracks.iter().find(|track| track.id == id).unwrap();
        assert_eq!(item.path, expected.path);
        assert_eq!(item.cue_track, None);
    }
    let text = fs::read_to_string(&destination)?;
    assert!(!text.contains('\r'));
    assert!(text.lines().any(|line| line == "./#first.wav"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn m3u_path_line_breaks_preserve_existing_destination_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    wav(&audio, 75)?;
    let scan = scan_paths(std::slice::from_ref(&audio), &[], false)?;
    assert!(scan.errors.is_empty(), "{:?}", scan.errors);
    let valid = track_from_record(1, scan.tracks().next().unwrap());
    let destination = directory.path().join("existing.m3u8");
    let original = b"original\0playlist\r\n\xff";
    fs::write(&destination, original)?;
    for name in ["bad\nextra.wav", "bad\rextra.wav"] {
        let mut invalid = valid.clone();
        invalid.id = 2;
        invalid.path = directory.path().join(name);
        fs::copy(&audio, &invalid.path)?;
        let error = export_m3u(
            &destination,
            [Ok(export_track(&valid)), Ok(export_track(&invalid))],
            &ScanCancellation::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("path containing CR or LF"));
        assert_eq!(fs::read(&destination)?, original);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn m3u_cue_sheet_line_breaks_preserve_existing_destination_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    wav(&directory.path().join("audio.wav"), 225)?;
    let destination = directory.path().join("existing.m3u8");
    let original = b"original\0playlist\r\n\xff";
    fs::write(&destination, original)?;
    for name in ["album\nextra.cue", "album\rextra.cue"] {
        let sheet = directory.path().join(name);
        two_track_sheet(&sheet)?;
        let scan = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
        assert!(scan.errors.is_empty(), "{:?}", scan.errors);
        assert_eq!(scan.track_count(), 3);
        let tracks = scan
            .tracks()
            .enumerate()
            .map(|(index, record)| Ok(export_track(&track_from_record(index as i64 + 1, record))));
        let error = export_m3u(&destination, tracks, &ScanCancellation::default()).unwrap_err();
        assert!(error.to_string().contains("path containing CR or LF"));
        assert_eq!(fs::read(&destination)?, original);
    }
    Ok(())
}

#[test]
fn m3u_roundtrip_resolves_parent_components() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("sub"))?;
    assert_playlist_roundtrip(&directory.path().join("sub/../playlist.m3u8"))
}

#[cfg(unix)]
#[test]
fn m3u_roundtrip_normalizes_parent_component_without_resolving_symlink() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let nested = directory.path().join("physical/nested");
    fs::create_dir_all(&nested)?;
    std::os::unix::fs::symlink(nested, directory.path().join("link"))?;
    let path = directory.path().join("link/../playlist.m3u8");
    assert_playlist_roundtrip(&path)?;
    assert!(directory.path().join("playlist.m3u8").is_file());
    assert!(!directory.path().join("physical/playlist.m3u8").exists());
    Ok(())
}

#[test]
fn m3u_roundtrip_preserves_ancestor_sibling_and_descendant_paths() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let parent = directory.path().join("playlists");
    fs::create_dir_all(parent.join("nested"))?;
    fs::create_dir_all(directory.path().join("music"))?;
    let paths = [
        directory.path().join("#ancestor.wav"),
        directory.path().join("music/sibling.wav"),
        parent.join("nested/descendant.wav"),
    ];
    for path in &paths {
        fs::write(path, b"playlist fixture")?;
    }
    let tracks = paths.iter().map(|path| {
        Ok(PlaylistExportTrack {
            path: path.clone(),
            title: String::new(),
            artist: String::new(),
            duration: None,
            cue: None,
        })
    });
    let destination = parent.join("playlist.m3u8");
    export_m3u(&destination, tracks, &ScanCancellation::default())?;
    let text = fs::read_to_string(&destination)?;
    let entries: Vec<_> = text.lines().filter(|line| !line.starts_with('#')).collect();
    assert_eq!(
        entries,
        [
            "../#ancestor.wav",
            "../music/sibling.wav",
            "nested/descendant.wav"
        ]
    );
    let imported = import_m3u(&destination)?;
    assert_eq!(
        imported
            .into_iter()
            .map(|item| item.path)
            .collect::<Vec<_>>(),
        paths
    );
    Ok(())
}

#[test]
fn playlist_import_never_traverses_directory_entries_or_type_changes() -> Result<()> {
    futures::executor::block_on(async {
        let directory = tempfile::tempdir()?;
        let root = directory.path().to_path_buf();
        let workers = &crate::execution::BlockingExecutor::new(4);
        let good = root.join("good.wav");
        let changed = root.join("changed.wav");
        wav(&good, 75)?;
        fs::create_dir(&changed)?;
        wav(&changed.join("hidden.wav"), 75)?;
        let playlist = root.join("list.m3u8");
        fs::write(&playlist, "good.wav\nchanged.wav\n")?;
        let cancellation = ScanCancellation::default();
        assert!(
            super::prepare_playlist_import(
                workers,
                &playlist,
                &cancellation,
                Some(std::slice::from_ref(&root))
            )
            .await
            .is_err()
        );
        fs::remove_dir_all(&changed)?;
        wav(&changed, 75)?;
        let prepared = super::prepare_playlist_import(
            workers,
            &playlist,
            &cancellation,
            Some(std::slice::from_ref(&root)),
        )
        .await?;
        fs::remove_file(&changed)?;
        fs::create_dir(&changed)?;
        wav(&changed.join("hidden.wav"), 75)?;
        let paths: Vec<_> = prepared
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let mut looked_up = Vec::new();
        let result = super::scan_paths_with_cues(
            workers,
            &paths,
            |paths| {
                looked_up.extend(paths.iter().cloned());
                std::future::ready(Ok(vec![None; paths.len()]))
            },
            super::ScanSettings {
                ffmpeg_enabled: false,
                library_roots: std::slice::from_ref(&root),
                max_depth: 4,
            },
            (&cancellation, Some(prepared.sheets)),
            |_, _, _, _, _| {},
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
        assert!(
            looked_up.is_empty(),
            "all import scopes are admitted before probing"
        );
        let normal = scan_uncached_paths(
            workers,
            std::slice::from_ref(&root),
            std::slice::from_ref(&root),
        )
        .await?;
        assert_eq!(
            normal.track_count(),
            2,
            "explicit library scans still recurse"
        );
        Ok(())
    })
}

#[test]
fn cue_import_rescan_relocation_and_copy_preserve_independent_user_state() -> Result<()> {
    futures::executor::block_on(async {
        let directory = tempfile::tempdir()?;
        let root = directory.path().to_path_buf();
        let old = root.join("old");
        fs::create_dir(&old)?;
        wav(&old.join("audio.wav"), 225)?;
        two_track_sheet(&old.join("album.cue"))?;
        let playlist = root.join("list.m3u8");
        fs::write(&playlist, "old/album.cue\n")?;
        let workers = &crate::execution::BlockingExecutor::new(4);
        let cancellation = ScanCancellation::default();
        let store = crate::store::Store::open(Path::new(":memory:"))?;
        let prepared = super::prepare_playlist_import(
            workers,
            &playlist,
            &cancellation,
            Some(std::slice::from_ref(&root)),
        )
        .await?;
        let paths: Vec<_> = prepared
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let imported = super::scan_paths_with_cues(
            workers,
            &paths,
            |paths| std::future::ready(Ok(vec![None; paths.len()])),
            super::ScanSettings {
                ffmpeg_enabled: false,
                library_roots: std::slice::from_ref(&root),
                max_depth: 4,
            },
            (&cancellation, Some(prepared.sheets)),
            |_, _, _, _, _| {},
        )
        .await?;
        store.apply_scan(&imported)?;
        let ids: Vec<_> = prepared
            .items
            .iter()
            .map(|item| {
                store
                    .track_id_for_identity(match item.cue_track {
                        Some(number) => crate::store::TrackIdentity::Cue(&item.path, number),
                        None => crate::store::TrackIdentity::File(&item.path),
                    })
                    .map(Option::unwrap)
            })
            .collect::<Result<_>>()?;
        store.set_favorite(&[ids[0]], true)?;
        store.edit_track(ids[0], Some("Personal"), None, None, None)?;
        store.enqueue_tracks(&ids)?;
        let queue = store.queue_page(None, 0, 10)?;
        let saved_playlist = store.create_playlist_with_tracks("Imported", &ids)?;
        fs::write(
            old.join("album.cue"),
            "TITLE \"New album\"\nPERFORMER \"New artist\"\nFILE \"audio.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"New raw title\"\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00\n",
        )?;
        let rescanned = scan_uncached_paths(
            workers,
            &[old.join("album.cue")],
            std::slice::from_ref(&root),
        )
        .await?;
        store.apply_scan(&rescanned)?;
        assert_eq!(store.track(ids[0])?.unwrap().title, "Personal");
        assert_eq!(store.track(ids[1])?.unwrap().artist, "New artist");
        let moved = root.join("moved");
        fs::rename(&old, &moved)?;
        let relocated = scan_uncached_paths(
            workers,
            &[moved.join("album.cue")],
            std::slice::from_ref(&root),
        )
        .await?;
        store.apply_scan(&relocated)?;
        for (index, id) in ids.iter().copied().enumerate() {
            assert_eq!(
                store.track_id_for_identity(crate::store::TrackIdentity::Cue(
                    &moved.join("album.cue"),
                    index as u32 + 1
                ))?,
                Some(id)
            );
        }
        let kept = store.track(ids[0])?.unwrap();
        assert!(kept.favorite);
        assert_eq!(kept.title, "Personal");
        assert_eq!(kept.path, moved.join("audio.wav"));
        let moved_queue = store.queue_page(None, 0, 10)?;
        assert_eq!(
            moved_queue
                .rows
                .iter()
                .map(|row| (row.id, row.track_id))
                .collect::<Vec<_>>(),
            queue
                .rows
                .iter()
                .map(|row| (row.id, row.track_id))
                .collect::<Vec<_>>()
        );
        assert_eq!(store.playlist_track_ids(saved_playlist, 10)?, ids);
        let copy = root.join("copy");
        fs::create_dir(&copy)?;
        fs::copy(moved.join("audio.wav"), copy.join("audio.wav"))?;
        fs::copy(moved.join("album.cue"), copy.join("album.cue"))?;
        store.apply_scan(
            &scan_uncached_paths(
                workers,
                &[copy.join("album.cue")],
                std::slice::from_ref(&root),
            )
            .await?,
        )?;
        let copy_id = store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(&copy.join("album.cue"), 1))?
            .unwrap();
        assert_ne!(copy_id, ids[0]);
        assert!(!store.track(copy_id)?.unwrap().favorite);
        assert_eq!(store.track(ids[0])?.unwrap().title, "Personal");
        Ok(())
    })
}

#[test]
fn import_retained_overflow_after_a_valid_sheet_aborts_the_whole_scan() -> Result<()> {
    futures::executor::block_on(async {
        let directory = tempfile::tempdir()?;
        let root = directory.path().to_path_buf();
        wav(&root.join("audio.wav"), 225)?;
        two_track_sheet(&root.join("a.cue"))?;
        two_track_sheet(&root.join("b.cue"))?;
        let playlist = root.join("list.m3u8");
        fs::write(&playlist, "a.cue\nb.cue\n")?;
        let workers = &crate::execution::BlockingExecutor::new(4);
        let cancellation = ScanCancellation::default();
        let prepared = super::prepare_playlist_import(
            workers,
            &playlist,
            &cancellation,
            Some(std::slice::from_ref(&root)),
        )
        .await?;
        let budget = prepared.sheets.budget.clone();
        let paths: Vec<_> = prepared
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let mut first_completed = false;
        let result = super::scan_paths_with_cues(
            workers,
            &paths,
            |paths| std::future::ready(Ok(vec![None; paths.len()])),
            super::ScanSettings {
                ffmpeg_enabled: false,
                library_roots: std::slice::from_ref(&root),
                max_depth: 4,
            },
            (&cancellation, Some(prepared.sheets)),
            |phase, processed, _, _, _| {
                if phase == ScanPhase::ReadingMetadata && processed == 1 && !first_completed {
                    first_completed = true;
                    budget.reserve(budget.remaining()).unwrap();
                }
            },
        )
        .await;
        assert!(first_completed);
        assert!(super::import_budget::ImportBudget::is_limit(
            &result.unwrap_err()
        ));
        assert_eq!(budget.remaining(), 0);
        Ok(())
    })
}

#[test]
fn import_source_fifo_substitution_is_fatal_without_blocking_or_recursion() -> Result<()> {
    futures::executor::block_on(async {
        let directory = tempfile::tempdir()?;
        let root = directory.path().to_path_buf();
        let audio = root.join("audio.wav");
        wav(&audio, 75)?;
        let playlist = root.join("list.m3u8");
        fs::write(&playlist, "audio.wav\n")?;
        let workers = &crate::execution::BlockingExecutor::new(4);
        let cancellation = ScanCancellation::default();
        let prepared = super::prepare_playlist_import(
            workers,
            &playlist,
            &cancellation,
            Some(std::slice::from_ref(&root)),
        )
        .await?;
        let paths: Vec<_> = prepared
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let mut replaced = false;
        let result = super::scan_paths_with_cues(
            workers,
            &paths,
            |paths| std::future::ready(Ok(vec![None; paths.len()])),
            super::ScanSettings {
                ffmpeg_enabled: false,
                library_roots: std::slice::from_ref(&root),
                max_depth: 4,
            },
            (&cancellation, Some(prepared.sheets)),
            |phase, _, _, _, _| {
                if phase == ScanPhase::ReadingMetadata && !replaced {
                    replaced = true;
                    fs::remove_file(&audio).unwrap();
                    let name =
                        std::ffi::CString::new(audio.as_os_str().as_encoded_bytes()).unwrap();
                    // SAFETY: the live C string is terminated and mkfifo retains no pointer.
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
            },
        )
        .await;
        assert!(replaced);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
        assert!(
            super::prepare_playlist_import(
                workers,
                &playlist,
                &cancellation,
                Some(std::slice::from_ref(&root))
            )
            .await
            .is_err()
        );
        Ok(())
    })
}
