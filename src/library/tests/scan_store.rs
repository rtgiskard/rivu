use super::*;

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
        let mut result = crate::library::scan_paths(
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
        let scan = crate::library::scan_paths(
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
            let result = crate::library::scan_paths(
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
    let result = crate::library::scan_paths(
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
        let cancelled = crate::library::scan_paths(
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
        let prepared = crate::library::prepare_playlist_import(
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
        let imported = crate::library::scan_paths_with_cues(
            workers,
            &paths,
            |paths| std::future::ready(Ok(vec![None; paths.len()])),
            crate::library::ScanSettings {
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
