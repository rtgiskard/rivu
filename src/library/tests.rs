use super::scan::{
    MAX_RECORDED_SCAN_UNCERTAINTIES, MAX_RECORDED_SKIPPED_DIRECTORIES, MAX_REPORTED_SCAN_ERRORS,
    UNCACHEABLE_MODIFIED_NS, cache_modified_ns,
};
use super::*;
use crate::model::{CueSegment, ScanPhase, Track};
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

fn scan_paths(paths: &[PathBuf], known: &[KnownFile], ffmpeg_enabled: bool) -> Result<ScanResult> {
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
        known,
        ffmpeg_enabled,
        &library_roots,
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )
}

fn scan_paths_at_depth(
    paths: &[PathBuf],
    known: &[KnownFile],
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
        known,
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
            &[],
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
            &[],
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
fn cancellation_during_a_hash_read_stops_at_the_chunk_boundary() {
    struct CancellingReader {
        cancellation: ScanCancellation,
        reads: usize,
    }
    impl Read for CancellingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.reads == 2 {
                self.cancellation.cancel();
            }
            if self.reads > 2 {
                return Ok(0);
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
    assert!(super::scan::hash_reader(&mut reader, &cancellation).is_err());
    assert_eq!(reader.reads, 2);
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
    let id = store.track_id_for_source(&audio, None)?.unwrap();
    let deleted_id = store.track_id_for_source(&deleted, None)?.unwrap();
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
    let id = store.track_id_for_source(&audio, None)?.unwrap();
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
fn failed_cue_read_preserves_owned_tracks_without_suppressing_full_audio() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().to_path_buf();
    let audio = root.join("audio.wav");
    let sheet = root.join("album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&root), &[], false)?)?;
    let first_id = store.track_id_for_source(&sheet, Some(1))?.unwrap();
    let second_id = store.track_id_for_source(&sheet, Some(2))?.unwrap();
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
    assert!(failed.suppressed_sources.is_empty());
    assert_eq!(failed.records.len(), 1);
    assert_eq!(failed.records[0].path, audio);
    assert!(failed.records[0].cue.is_none());
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
fn failed_cue_source_not_found_keeps_segments_until_the_sheet_is_deleted() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let sheet = directory.path().join("album.cue");
    wav(&audio, 225)?;
    two_track_sheet(&sheet)?;
    let store = crate::store::Store::open(Path::new(":memory:"))?;
    store.apply_scan(&scan_paths(std::slice::from_ref(&sheet), &[], false)?)?;
    let id = store.track_id_for_source(&sheet, Some(1))?.unwrap();
    fs::remove_file(&audio)?;
    let failed = scan_paths(std::slice::from_ref(&sheet), &[], false)?;
    assert!(failed.records.is_empty());
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
    let audio_id = store.track_id_for_source(&audio, None)?.unwrap();
    let cue_id = store.track_id_for_source(&sheet, Some(1))?.unwrap();
    let deleted_id = store.track_id_for_source(&deleted, None)?.unwrap();
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
    assert_ne!(store.track_id_for_source(&copy, None)?.unwrap(), audio_id);
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
    let id = store.track_id_for_source(&audio, None)?.unwrap();
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
    assert!(result.records.is_empty());
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
        &[],
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
        &[],
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
    assert_eq!(result.records.len(), 1);
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
        &[],
        false,
        &[root.clone(), other_root],
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.roots, [root]);
    assert_eq!(result.records.len(), 2);
    assert_eq!(result.records[0].path, tree.join("audio.wav"));
    assert_eq!(result.records[1].path, remote);
    assert!(
        result
            .records
            .iter()
            .all(|record| record.cue.as_ref().unwrap().sheet == sheet)
    );
    let mut suppressed = vec![tree.join("audio.wav"), remote];
    suppressed.sort();
    assert_eq!(result.suppressed_sources, suppressed);
    Ok(())
}

#[test]
fn scan_progress_reports_deduplicated_candidates_and_suppressed_tasks() -> Result<()> {
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
        &[],
        false,
        &library_roots,
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |phase, processed, total, errors, path| {
            events.push((phase, processed, total, errors, path.map(Path::to_path_buf)));
        },
    )?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.records.len(), 2);
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
        .records
        .iter()
        .map(|record| record.path.clone())
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
    assert_eq!(direct.records.len(), 1);
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
    let id = store.track_id_for_source(&deep, None)?.unwrap();
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
    assert_ne!(store.track_id_for_source(&copy, None)?.unwrap(), id);
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
        &[],
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
        &[],
        false,
        &[logical_path(inside.path())?, logical_path(outside.path())?],
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |_, _, _, _, _| {},
    )?;
    assert!(control.errors.is_empty(), "{:?}", control.errors);
    assert_eq!(control.records[0].path, logical_path(&source)?);
    assert!(control.records[0].cue.is_some());
    let mut events = Vec::new();
    let result = super::scan_paths(
        std::slice::from_ref(&sheet),
        &[],
        false,
        &library_roots,
        crate::config::Config::default().scan_max_depth,
        &ScanCancellation::default(),
        |phase, processed, total, errors, path| {
            events.push((phase, processed, total, errors, path.map(Path::to_path_buf)));
        },
    )?;
    assert!(result.records.is_empty());
    assert_eq!(result.errors.len(), 1);
    assert!(result.suppressed_sources.is_empty());
    let metadata = events
        .iter()
        .rfind(|(phase, ..)| matches!(phase, &ScanPhase::ReadingMetadata))
        .unwrap();
    assert_eq!(metadata.1, 1);
    assert_eq!(metadata.2, Some(1));
    assert_eq!(metadata.3, 1);
    Ok(())
}

fn known_records(records: &[ScanRecord]) -> Vec<KnownFile> {
    records
        .iter()
        .enumerate()
        .map(|(index, record)| KnownFile {
            track_id: index as i64 + 1,
            path: record.path.clone(),
            size: record.size,
            modified_ns: record.modified_ns,
            fingerprint: record.fingerprint.clone(),
            media: Some(record.media.clone()),
            cue: record.cue.clone(),
        })
        .collect()
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
            track_id: 1,
            path: PathBuf::from("audio.wav"),
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
    let id = store.track_id_for_source(&audio, None)?.unwrap();
    let known = store.known_files()?;
    assert_eq!(known[0].modified_ns, -2_000_000_000);

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
    assert_eq!(store.track_id_for_source(&audio, None)?, Some(id));
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
    let known = known_records(&original.records);

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

fn track_from_record(id: i64, record: &ScanRecord) -> Track {
    Track {
        id,
        path: record.path.clone(),
        fingerprint: record.fingerprint.clone(),
        title: record.media.title.clone(),
        artist: record.media.artist.clone(),
        album: record.media.album.clone(),
        duration: record.media.duration,
        bitrate_bps: record.media.bitrate_bps,
        track_number: record.media.track_number,
        disc_number: record.media.disc_number,
        bits_per_sample: record.media.bits_per_sample,
        release_date: record.media.release_date.clone(),
        favorite: false,
        decoder_hint: Default::default(),
        codec: record.media.codec.clone(),
        channels: record.media.channels,
        sample_rate: record.media.sample_rate,
        cue: record.cue.clone(),
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
    assert_eq!(result.records.len(), 2);
    let first = &result.records[0];
    let second = &result.records[1];
    assert_eq!(first.path, logical_path(&audio)?);
    assert_eq!(second.path, first.path);
    assert_eq!(first.media.title, "First");
    assert_eq!(first.media.artist, "Track artist");
    assert_eq!(second.media.title, "Track 02");
    assert_eq!(second.media.artist, "Sheet artist");
    assert_eq!(first.media.album, "Album");
    assert_eq!(first.media.duration, Some(1.0));
    assert_eq!(second.media.duration, Some(2.0));
    assert_eq!(first.media.bitrate_bps, Some(120_000));
    assert_eq!(second.media.bitrate_bps, first.media.bitrate_bps);
    assert_eq!(first.media.bits_per_sample, Some(16));
    assert_eq!(first.media.track_number, Some(1));
    assert_eq!(second.media.track_number, Some(2));
    assert_eq!(
        first.cue.as_ref().unwrap(),
        &CueSegment {
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
    assert_eq!(result.records.len(), 4);
    assert_eq!(
        result
            .records
            .iter()
            .map(|record| record.cue.as_ref().unwrap().number)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        result
            .records
            .iter()
            .map(|record| record.media.duration)
            .collect::<Vec<_>>(),
        [Some(1.0), Some(1.0), Some(2.0), Some(1.0)]
    );
    assert_eq!(result.records[1].cue.as_ref().unwrap().end_frame, None);
    assert_eq!(result.records[2].cue.as_ref().unwrap().start_frame, 0);
    assert_eq!(result.suppressed_sources.len(), 2);
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
    let known = known_records(&original.records);
    let full = scan_paths(std::slice::from_ref(&audio), &known, false)?;
    assert!(full.errors.is_empty(), "{:?}", full.errors);
    assert_eq!(full.records.len(), 1);
    assert_eq!(full.records[0].media.duration, Some(3.0));
    assert!(full.records[0].cue.is_none());
    fs::write(
        &sheet,
        "TITLE \"Edited album\"\nFILE \"audio.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"Renamed\"\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:02:00\n",
    )?;
    for cache in [&known, &known_records(&full.records)] {
        let rescanned = scan_paths(std::slice::from_ref(&sheet), cache, false)?;
        assert!(rescanned.errors.is_empty(), "{:?}", rescanned.errors);
        assert_eq!(rescanned.records.len(), 2);
        assert_eq!(rescanned.records[0].media.title, "Renamed");
        assert_eq!(rescanned.records[0].media.album, "Edited album");
        assert_eq!(
            rescanned.records[0].media.artist,
            full.records[0].media.artist
        );
        assert_eq!(rescanned.records[0].media.duration, Some(2.0));
        assert_eq!(rescanned.records[1].media.duration, Some(1.0));
        assert_eq!(
            rescanned.records[0].fingerprint,
            original.records[0].fingerprint
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
        assert!(result.records.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert!(result.suppressed_sources.is_empty());
        let directory_scan = scan_paths(&[directory.path().to_path_buf()], &[], false)?;
        assert_eq!(directory_scan.records.len(), 1);
        assert!(directory_scan.records[0].cue.is_none());
        assert!(directory_scan.suppressed_sources.is_empty());
    }
    Ok(())
}

#[test]
fn cue_directory_suppresses_sources_but_explicit_audio_survives() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let audio = directory.path().join("audio.wav");
    let sheet = directory.path().join("album.cue");
    wav(&audio, 225)?;
    wav(&directory.path().join("unrelated.wav"), 75)?;
    two_track_sheet(&sheet)?;
    let result = scan_paths(&[directory.path().to_path_buf(), sheet.clone()], &[], false)?;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.records.len(), 3);
    assert_eq!(
        result
            .records
            .iter()
            .filter(|record| record.cue.is_none())
            .count(),
        1
    );
    assert_eq!(result.suppressed_sources, [logical_path(&audio)?]);
    let explicit = scan_paths(
        &[directory.path().to_path_buf(), audio.clone(), sheet, audio],
        &[],
        false,
    )?;
    assert!(explicit.errors.is_empty(), "{:?}", explicit.errors);
    assert_eq!(explicit.records.len(), 4);
    assert_eq!(
        explicit
            .records
            .iter()
            .filter(|record| record.cue.is_none())
            .count(),
        2
    );
    assert!(explicit.suppressed_sources.is_empty());
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
    assert_eq!(result.records.len(), 4);
    assert!(result.suppressed_sources.is_empty());
    let full = result
        .records
        .iter()
        .find(|record| record.cue.is_none())
        .unwrap();
    assert_eq!(full.path, audio);
    assert_eq!(full.media.duration, Some(3.0));
    assert_eq!(full.media.title, original.records[0].media.title);
    assert_eq!(full.fingerprint, original.records[0].fingerprint);
    let segments: Vec<_> = result
        .records
        .iter()
        .filter(|record| record.cue.is_some())
        .collect();
    assert_eq!(segments[0].cue.as_ref().unwrap().sheet, first_sheet);
    assert_eq!(segments[0].media.duration, Some(1.0));
    assert_eq!(segments[1].media.duration, Some(2.0));
    assert_eq!(segments[2].cue.as_ref().unwrap().sheet, second_sheet);
    assert_eq!(segments[2].media.title, "Other track");
    assert_eq!(segments[2].media.album, "Other album");
    assert_eq!(segments[2].media.duration, Some(2.0));
    assert!(
        segments
            .iter()
            .all(|record| record.fingerprint == full.fingerprint)
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
    assert!(result.records.is_empty());
    assert!(result.suppressed_sources.is_empty());
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

fn playlist_tracks<'a>(
    track_ids: &'a [i64],
    tracks: &'a [Track],
) -> impl Iterator<Item = Result<Track>> + 'a {
    track_ids.iter().map(|id| {
        tracks
            .iter()
            .find(|track| track.id == *id)
            .cloned()
            .context("Missing fixture track")
    })
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
        scan.records
            .iter()
            .all(|record| record.path == alias.join("audio.wav"))
    );
    let tracks: Vec<_> = scan
        .records
        .iter()
        .enumerate()
        .map(|(index, record)| track_from_record(index as i64 + 1, record))
        .collect();
    let mut sequence: Vec<_> = tracks
        .iter()
        .filter(|track| track.cue.is_some())
        .map(|track| track.id)
        .collect();
    sequence.push(tracks.iter().find(|track| track.cue.is_none()).unwrap().id);
    let path = alias.join("playlist.m3u8");
    export_m3u(&path, playlist_tracks(&sequence, &tracks))?;
    let imported = import_playlist(&path)?;
    let expected = [sequence[0], sequence[1], *sequence.last().unwrap()];
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
        1
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
        .records
        .iter()
        .enumerate()
        .map(|(index, record)| track_from_record(index as i64 + 1, record))
        .collect();
    let path = directory.path().join("existing.m3u8");
    fs::write(&path, "original playlist\n")?;
    for sequence in [vec![1], vec![2], vec![2, 1]] {
        assert!(export_m3u(&path, playlist_tracks(&sequence, &tracks)).is_err());
        assert_eq!(fs::read_to_string(&path)?, "original playlist\n");
    }
    // A sheet changed outside Rivu must not silently change exported ranges.
    let stale_sequence = [1, 2];
    fs::write(
        &sheet,
        fs::read_to_string(&sheet)?.replace("00:01:00", "00:02:00"),
    )?;
    assert!(export_m3u(&path, playlist_tracks(&stale_sequence, &tracks)).is_err());
    assert_eq!(fs::read_to_string(&path)?, "original playlist\n");
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
    export_m3u(path, playlist_tracks(&sequence, &tracks))?;
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
    assert_eq!(imported[0].name.as_deref(), Some("other song.wav"));
    assert_eq!(imported[1].name.as_deref(), Some("Artist - #song.wav"));
    Ok(())
}

#[test]
fn m3u_roundtrip_preserves_hash_paths_and_first_track_order() -> Result<()> {
    let directory = tempfile::tempdir()?;
    assert_playlist_roundtrip(&directory.path().join("playlist.m3u8"))
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
