use super::*;
use crate::core::catalog::ScanFinished;

// Scan commits use another connection; all catalog task tests need a real DB.
fn fixture() -> (tempfile::TempDir, Core) {
    fixture_with_database(Some(10.0))
}

#[test]
fn committed_scan_invalidates_before_failed_refresh() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    let track = core.track(1).unwrap();
    let path = track.path.clone();
    core.state.queue.current_id = Some(1);
    core.state.current_track = Some(Arc::new(track));
    core.save_queue().unwrap();
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    core.command(Command::CreatePlaylistWithTracks {
        name: "Retained".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_scan_refresh AFTER UPDATE OF missing ON tracks WHEN NEW.id=1
         BEGIN UPDATE tracks SET channels='unreadable' WHERE id=1; END;",
        )
        .unwrap();
    let before = core.state.query_revisions();
    let status = save_prepared_scan(
        &mut core,
        ScanFinished {
            other_tracks_check: false,
            id: 1,
            result: Ok(ScanResult {
                roots: vec![path],
                ..ScanResult::default()
            }),
            import: None,
            progress: ScanProgress {
                phase: ScanPhase::ReadingMetadata,
                processed: 1,
                total: Some(1),
                errors: 0,
                path: None,
            },
        },
    );
    assert_eq!(status.progress.phase, ScanPhase::Failed);
    assert!(status.library_committed);
    assert!(status.error.as_deref().unwrap().contains("Scan committed"));
    let missing: bool = connection
        .query_row("SELECT missing FROM tracks WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(missing);
    let committed = core.state.query_revisions();
    assert_eq!(committed.library, before.library + 1);
    assert_eq!(committed.structure, before.structure);
    assert_eq!(committed.playlist, before.playlist);
    assert!(Arc::ptr_eq(
        &core.state.library.playlist_entry_revisions,
        &scopes
    ));
    assert!(committed.queue > before.queue);
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
}

#[test]
fn scan_count_failure_preserves_playlist_domains_and_still_refreshes_queue() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    core.command(Command::CreatePlaylist {
        name: "Retained".into(),
    })
    .unwrap();
    let history = Arc::clone(&core.state.library.history);
    let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
    let before = core.state.query_revisions();
    let media = core
        .store
        .stored_scan_files(&[directory.path().to_path_buf()])
        .unwrap()[0]
        .media
        .clone()
        .unwrap();
    let mut impacts = core
        .store
        .apply_scan(&ScanResult {
            records: (vec![ScanRecord {
                path: directory.path().join("committed.wav"),
                size: 1,
                modified_ns: 1,
                fingerprint: Some(blake3::hash(b"committed fingerprint").to_hex().to_string()),
                media,
            }])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    assert!(impacts.library && impacts.structure);
    // Include a queued display change in the committed fixture facts. Queue
    // invalidation must survive the later count projection failure.
    let edit = core
        .store
        .edit_track(1, Some("Committed queued title"), None, None, None)
        .unwrap();
    impacts.queue |= edit.queue;
    impacts.albums |= edit.albums;
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    // Remove the count's source after the insertion commits.
    connection
        .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE tracks;")
        .unwrap();
    let error = core.apply_scan_impacts(impacts).unwrap_err();
    assert!(format!("{error:#}").contains("no such table: tracks"));
    let after = core.state.query_revisions();
    assert_ne!(after.library, before.library);
    assert_ne!(after.structure, before.structure);
    assert_eq!(
        after.duplicate_groups != before.duplicate_groups,
        impacts.duplicate_groups
    );
    assert_eq!(
        after.duplicate_members != before.duplicate_members,
        impacts.duplicate_members
    );
    assert_eq!(after.playlist, before.playlist);
    assert!(Arc::ptr_eq(
        &core.state.library.playlist_entry_revisions,
        &scopes
    ));
    assert_eq!(core.state.library.track_total, 4);
    assert_eq!(core.state.library.playlist_total, 1);
    assert!(Arc::ptr_eq(&core.state.library.history, &history));
    assert_ne!(after.queue, before.queue);
    assert_queue_saved(&core);
}

#[test]
fn startup_and_catalog_refresh_retain_the_same_recent_history() {
    let (directory, mut core) = fixture();
    let media = core
        .store
        .stored_scan_files(&[directory.path().to_path_buf()])
        .unwrap()[0]
        .media
        .clone()
        .unwrap();
    let result = ScanResult {
        roots: vec![directory.path().to_path_buf()],
        records: ((1..=MAX_QUERY_ROWS + 1)
            .map(|id| ScanRecord {
                path: directory.path().join(format!("history-{id}.wav")),
                size: 1,
                modified_ns: 1,
                fingerprint: None,
                media: media.clone(),
            })
            .collect::<Vec<_>>())
        .into_iter()
        .map(std::sync::Arc::new)
        .collect(),
        ..ScanResult::default()
    };
    let impacts = core.store.apply_scan(&result).unwrap();
    let ids: Vec<_> = core
        .store
        .stored_scan_files(&[directory.path().to_path_buf()])
        .unwrap()
        .into_iter()
        .filter(|known| {
            known
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("history-")
        })
        .map(|known| known.track_id)
        .collect();
    for (index, &id) in ids.iter().enumerate() {
        core.store.mark_played(id, index as i64 + 1).unwrap();
    }
    let expected: Vec<_> = ids.iter().rev().take(MAX_QUERY_ROWS).copied().collect();
    core.load_initial_state().unwrap();
    let initial: Vec<_> = core
        .state
        .library
        .history
        .iter()
        .map(|entry| entry.track_id)
        .collect();
    assert_eq!(initial, expected);
    core.apply_scan_impacts(impacts).unwrap();
    let refreshed: Vec<_> = core
        .state
        .library
        .history
        .iter()
        .map(|entry| entry.track_id)
        .collect();
    assert_eq!(refreshed, expected);
}

#[test]
fn unchanged_scans_keep_query_results_valid_and_still_import_playlists() {
    let (directory, mut core) = fixture();
    core.state.queue.current_id = Some(1);
    core.save_queue().unwrap();
    core.refresh_queue_tracks().unwrap();
    core.command(Command::CreatePlaylistWithTracks {
        name: "retained".into(),
        track_ids: vec![2, 3],
    })
    .unwrap();
    let retained = core.state.playlist_query_revisions(1);
    let result = ScanResult {
        roots: vec![directory.path().to_path_buf()],
        records: (core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()
            .into_iter()
            .map(|known| ScanRecord {
                path: known.path,
                size: known.size,
                modified_ns: known.modified_ns,
                fingerprint: known.fingerprint,
                media: known.media.unwrap(),
            })
            .collect::<Vec<_>>())
        .into_iter()
        .map(std::sync::Arc::new)
        .collect(),
        ..ScanResult::default()
    };
    let before = core.state.query_revisions();
    let current = core.state.current_track.clone().unwrap();
    for import in [false, true] {
        let scan = ScanFinished {
            other_tracks_check: false,
            id: 0,
            result: Ok(result.clone()),
            import: import.then(|| {
                (
                    "unchanged import".into(),
                    vec![M3uItem {
                        path: directory.path().join("1.wav"),
                        cue_track: None,
                    }],
                )
            }),
            progress: ScanProgress {
                phase: ScanPhase::Completed,
                processed: 4,
                total: Some(4),
                errors: 0,
                path: None,
            },
        };
        let status = save_prepared_scan(&mut core, scan);
        assert_eq!(status.progress.phase, ScanPhase::Completed);
        assert!(status.library_committed);
        assert_eq!(status.playlist_id.is_some(), import);
        let after = core.state.query_revisions();
        assert_eq!(after.library, before.library);
        assert_eq!(after.structure, before.structure);
        assert_eq!(after.queue, before.queue);
        let other = core.query_response(&Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 2,
            expected: Some(retained),
        });
        assert!(other.result.is_ok());
        assert_eq!(other.revisions.playlist_entries, retained.playlist_entries);
        if import {
            assert_ne!(after.playlist, before.playlist);
            assert_ne!(after.duplicate_members, before.duplicate_members);
            let playlist = core.store.playlist_summary_page(None, 1, 1).unwrap().rows[0].id;
            assert_eq!(core.store.playlist_track_ids(playlist, 10).unwrap(), [1]);
        } else {
            assert_eq!(after, before);
        }
        assert_eq!(core.state.current_track.as_deref(), Some(current.as_ref()));
        assert!(
            core.view(&Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: None,
                limit: 2,
                expected: Some(before),
            })
            .is_ok()
        );
    }
}

#[test]
fn invalid_import_names_are_rejected_before_scan_acceptance() {
    let (directory, mut core) = fixture();
    let source = directory.path().join("new.wav");
    write_pcm(&source);
    let before = core.state.query_revisions();
    let next_scan = core.next_scan_id;
    for name in [
        "",
        " ",
        "\u{2003}",
        "\tMix",
        "Mix\n",
        "Mix\u{7f}",
        "Mix\u{85}",
    ] {
        let path = directory.path().join("valid.m3u8");
        std::fs::write(&path, "new.wav\n").unwrap();
        let error = core
            .command(Command::ImportPlaylist {
                path,
                name: Some(name.into()),
            })
            .unwrap_err();
        assert!(error.to_string().contains("name"));
    }
    for filename in [" .m3u8", "\u{2003}.m3u8", "bad\nname.m3u8"] {
        let path = directory.path().join(filename);
        std::fs::write(&path, "new.wav\n").unwrap();
        let error = core
            .command(Command::ImportPlaylist { path, name: None })
            .unwrap_err();
        assert!(error.to_string().contains("name"));
    }
    let error = core
        .command(Command::ImportPlaylist {
            path: PathBuf::new(),
            name: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("name"));
    assert!(core.active_scan.is_none());
    assert!(core.scan_results.is_empty());
    assert_eq!(core.next_scan_id, next_scan);
    assert_eq!(core.state.query_revisions(), before);
    assert_eq!(core.store.track_count().unwrap(), 4);
    assert!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&source))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        core.store.playlist_summary_page(None, 0, 0).unwrap().total,
        0
    );
}

#[test]
fn imports_preserve_valid_explicit_and_default_name_whitespace() {
    for explicit in [false, true] {
        let (directory, mut core) = fixture();
        write_pcm(&directory.path().join("new.wav"));
        let name = "  Mix \u{2003} ";
        let path = directory.path().join(format!("{name}.m3u8"));
        std::fs::write(&path, "new.wav\n").unwrap();
        core.command(Command::ImportPlaylist {
            path,
            name: explicit.then(|| name.into()),
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let playlists = core.store.playlist_summary_page(None, 0, 10).unwrap();
        assert_eq!(playlists.total, 1);
        assert_eq!(playlists.rows[0].name, name);
        assert_eq!(playlists.rows[0].entry_count, 1);
    }
}

#[test]
fn import_task_errors_distinguish_scan_save_from_playlist_creation_failure() {
    for scan_saved in [false, true] {
        let (directory, mut core) = fixture_with_database(Some(10.0));
        let source = directory.path().join("new.wav");
        core.create_playlist_with_tracks("Existing".into(), vec![1])
            .unwrap();
        let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
        write_pcm(&source);
        let path = directory.path().join("mix.m3u8");
        std::fs::write(&path, "new.wav\n").unwrap();
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection
            .execute_batch(if scan_saved {
                "CREATE TRIGGER reject_import BEFORE INSERT ON playlists
                 BEGIN SELECT RAISE(ABORT, 'playlist write refused'); END;"
            } else {
                "CREATE TRIGGER reject_import BEFORE INSERT ON tracks
                 BEGIN SELECT RAISE(ABORT, 'scan write refused'); END;"
            })
            .unwrap();
        core.command(Command::ImportPlaylist { path, name: None })
            .unwrap();
        let id = core.active_scan.as_ref().unwrap().status.id;
        assert!(!settle_scan(&mut core));
        let ViewResponse::ScanTask(task) = core.view(&Query::ScanTask { id }).unwrap() else {
            panic!("Expected task");
        };
        let error = task.error.as_deref().unwrap();
        assert_eq!(task.library_committed, scan_saved);
        assert_eq!(task.playlist_id, None);
        if scan_saved {
            assert!(task.message.contains("Scan saved"));
            assert!(task.message.contains("playlist creation"));
            assert!(error.contains("playlist write refused"));
        } else {
            assert!(task.message.contains("save scan"));
            assert!(task.message.contains("playlist not created"));
            assert!(error.contains("scan write refused"));
        }
        assert_eq!(
            core.store
                .track_id_for_identity(crate::store::TrackIdentity::File(&source))
                .unwrap()
                .is_some(),
            scan_saved
        );
        assert_eq!(
            core.state.library.track_total,
            if scan_saved { 5 } else { 4 }
        );
        assert_eq!(
            core.store.playlist_summary_page(None, 0, 0).unwrap().total,
            1
        );
        assert!(Arc::ptr_eq(
            &scopes,
            &core.state.library.playlist_entry_revisions
        ));
    }
}

#[test]
fn playlist_refresh_failure_preserves_both_committed_results() {
    let (directory, mut core) = fixture();
    core.create_playlist_with_tracks("Existing".into(), vec![1])
        .unwrap();
    let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
    core.state.queue.current_id = Some(1);
    core.save_queue().unwrap();
    core.refresh_queue_tracks().unwrap();
    let source = directory.path().join("imported.wav");
    write_pcm(&source);
    let path = directory.path().join("mix.m3u8");
    std::fs::write(&path, "imported.wav\n").unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_import_refresh AFTER INSERT ON playlists
             BEGIN UPDATE tracks SET channels='unreadable' WHERE id=1; END;",
        )
        .unwrap();
    let before = core.state.query_revisions();
    core.command(Command::ImportPlaylist { path, name: None })
        .unwrap();
    assert!(!settle_scan(&mut core));
    let status = core.scan_results.back().unwrap();
    assert_eq!(status.progress.phase, ScanPhase::Failed);
    assert!(status.library_committed);
    let playlist_id = status
        .playlist_id
        .expect("Committed playlist identity survives refresh failure");
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("refreshing views failed")
    );
    assert_eq!(
        core.store
            .playlist_track_ids(playlist_id, 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(core.state.library.track_total, 5);
    assert_eq!(core.state.library.playlist_total, 2);
    assert!(
        core.state
            .library
            .playlist_entry_revisions
            .contains_key(&playlist_id)
    );
    for (id, revision) in scopes.iter() {
        assert_eq!(
            core.state.library.playlist_entry_revisions.get(id),
            Some(revision)
        );
    }
    assert_eq!(scopes.len(), 1);
    assert!(!scopes.contains_key(&playlist_id));
    assert_eq!(core.state.library.playlist_entry_revisions.len(), 2);
    let imported_revision = core.state.library.playlist_entry_revisions[&playlist_id];
    assert!(
        scopes
            .values()
            .all(|revision| *revision < imported_revision)
    );
    let after = core.state.query_revisions();
    assert_ne!(after.library, before.library);
    assert_ne!(after.playlist, before.playlist);
}

#[test]
fn failed_playlist_import_keeps_committed_scan_visible() {
    let (directory, mut core) = fixture();
    let before = core.state.query_revisions();
    let mut media = core
        .store
        .stored_scan_files(&[directory.path().to_path_buf()])
        .unwrap()
        .into_iter()
        .find(|known| known.track_id == 1)
        .unwrap()
        .media
        .unwrap();
    media.title = "Updated by scan".into();
    let scan = ScanFinished {
        other_tracks_check: false,
        id: 0,
        result: Ok(ScanResult {
            records: (vec![ScanRecord {
                path: directory.path().join("1.wav"),
                size: 1,
                modified_ns: 2,
                fingerprint: None,
                media,
            }])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
            ..ScanResult::default()
        }),
        import: Some(("empty".into(), vec![])),
        progress: ScanProgress {
            phase: ScanPhase::Completed,
            processed: 1,
            total: Some(1),
            errors: 0,
            path: None,
        },
    };
    let status = save_prepared_scan(&mut core, scan);
    assert!(status.library_committed);
    assert_eq!(status.playlist_id, None);
    let error = status.error.as_deref().unwrap();
    assert!(error.contains("Scan saved"));
    assert!(error.contains("playlist creation"));
    assert_eq!(
        core.store.queue_page(None, 0, 1).unwrap().rows[0]
            .track
            .title,
        "Updated by scan"
    );
    assert!(
        core.view(&Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: Some(1),
            limit: 2,
            expected: Some(before),
        })
        .is_err()
    );
    assert_eq!(core.state.library.track_total, 4);
    assert_eq!(core.state.library.playlist_total, 0);
}

#[test]
fn failed_cue_import_never_reuses_old_entries_but_keeps_readable_items() {
    for readable in [false, true] {
        let (directory, mut core) = fixture();
        let sheet = directory.path().join("album.cue");
        let media = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()
            .into_iter()
            .find(|file| file.track_id == 1)
            .unwrap()
            .media
            .unwrap();
        let old = ScanRecord {
            path: directory.path().join("1.wav"),
            size: 1,
            modified_ns: 1,
            fingerprint: None,
            media: media.clone(),
        };
        let old = library::CueRecord::registered(
            Arc::new(old),
            CueSegment {
                sheet: sheet.clone(),
                number: 1,
                start_frame: 0,
                end_frame: Some(75),
            },
            library::RegisteredCueMetadata {
                title: media.title.clone(),
                artist: media.artist.clone(),
                album: media.album.clone(),
                track_number: media.track_number,
                disc_number: media.disc_number,
                release_date: media.release_date.clone(),
            },
            media.duration,
        );
        let impacts = core
            .store
            .apply_scan(&ScanResult {
                segments: vec![old],
                ..ScanResult::default()
            })
            .unwrap();
        core.apply_scan_impacts(impacts).unwrap();
        let old_id = core
            .store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))
            .unwrap()
            .unwrap();
        let path = directory.path().join("2.wav");
        let scan = ScanFinished {
            other_tracks_check: false,
            id: 0,
            result: Ok(ScanResult {
                records: (if readable {
                    vec![ScanRecord {
                        path: path.clone(),
                        size: 1,
                        modified_ns: 1,
                        fingerprint: None,
                        media,
                    }]
                } else {
                    vec![]
                })
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
                errors: vec!["CUE sheet could not be read".into()],
                ..ScanResult::default()
            }),
            import: Some((
                "partial".into(),
                vec![
                    M3uItem {
                        path: sheet,
                        cue_track: Some(1),
                    },
                    M3uItem {
                        path,
                        cue_track: None,
                    },
                ],
            )),
            progress: ScanProgress {
                phase: ScanPhase::Completed,
                processed: 2,
                total: Some(2),
                errors: 1,
                path: None,
            },
        };
        let status = save_prepared_scan(&mut core, scan);
        assert_eq!(status.progress.phase, ScanPhase::Failed);
        assert!(status.library_committed);
        assert!(core.store.track(old_id).unwrap().is_some());
        if readable {
            assert!(status.playlist_id.is_some());
            let rows = core
                .store
                .playlist_entries_page(1, None, 0, MAX_QUERY_ROWS)
                .unwrap()
                .rows;
            assert_eq!(rows.iter().map(|row| row.track_id).collect::<Vec<_>>(), [2]);
        } else {
            assert_eq!(status.playlist_id, None);
            assert_eq!(core.state.library.playlist_total, 0);
        }
    }
}

#[test]
fn scan_result_dismissal_preserves_errors_and_active_scans() {
    let (_directory, mut core) = fixture();
    for phase in [
        ScanPhase::Completed,
        ScanPhase::Failed,
        ScanPhase::Cancelled,
    ] {
        core.state.system.scan_progress = Some(ScanProgress {
            phase,
            processed: 2,
            total: Some(2),
            errors: usize::from(phase == ScanPhase::Failed),
            path: None,
        });
        core.state.system.scan_message = "Scan result".into();
        core.state.system.last_error = Some("Unreadable source".into());
        core.state.system.scanning = true;
        let progress = core.state.system.scan_progress.clone();
        core.command(Command::DismissScanResult).unwrap();
        assert_eq!(core.state.system.scan_progress, progress);
        assert_eq!(core.state.system.scan_message, "Scan result");

        core.state.system.scanning = false;
        core.command(Command::DismissScanResult).unwrap();
        assert!(core.state.system.scan_progress.is_none());
        assert!(core.state.system.scan_message.is_empty());
        assert_eq!(
            core.state.system.last_error.as_deref(),
            Some("Unreadable source")
        );
    }
}

fn settle_scan(core: &mut Core) -> bool {
    while core.active_scan.is_some() {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        core.scan_event(event);
    }
    core.scan_results.back().unwrap().progress.phase == ScanPhase::Completed
}

#[test]
fn resumed_import_preserves_latest_playlist_tokens_and_snapshot_isolation() {
    for shutdown in [false, true] {
        let (directory, mut core) = fixture();
        core.create_playlist_with_tracks("Existing".into(), vec![1])
            .unwrap();
        let existing_id = core.store.playlist_summary_page(None, 0, 1).unwrap().rows[0].id;
        let old_scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
        write_pcm(&directory.path().join("imported.wav"));
        let path = directory.path().join("mix.m3u8");
        std::fs::write(&path, "imported.wav\n").unwrap();
        core.import_playlist(path, "Imported".into()).unwrap();
        let finished = loop {
            match core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ScanEvent::Finished(scan) => break scan,
                event => core.scan_event(event),
            }
        };
        // A relocation may defer an otherwise completed traversal. Playlist
        // changes before resuming must be in the snapshot Saving captures.
        core.active_scan.as_mut().unwrap().pending_save = Some(finished);
        core.add_playlist(existing_id, vec![2]).unwrap();
        core.create_playlist("Created while waiting".into())
            .unwrap();
        let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
        assert_ne!(scopes[&existing_id], old_scopes[&existing_id]);
        let before = core.state.query_revisions();
        core.resume_pending_scan();
        assert_eq!(
            core.active_scan.as_ref().unwrap().status.progress.phase,
            ScanPhase::Saving
        );
        if shutdown {
            core.shutdown_scan();
        } else {
            assert!(settle_scan(&mut core));
        }
        let status = core.scan_results.back().unwrap();
        assert_eq!(status.progress.phase, ScanPhase::Completed);
        assert!(status.library_committed);
        let imported_id = status.playlist_id.unwrap();
        let revisions = &core.state.library.playlist_entry_revisions;
        for (id, revision) in scopes.iter() {
            assert_eq!(revisions.get(id), Some(revision));
        }
        let imported_revision = revisions[&imported_id];
        assert!(
            scopes
                .values()
                .all(|revision| *revision < imported_revision)
        );
        assert_eq!(revisions.len(), 3);
        assert_eq!(scopes.len(), 2);
        assert!(!scopes.contains_key(&imported_id));
        assert_eq!(old_scopes.len(), 1);
        assert_ne!(core.state.query_revisions().playlist, before.playlist);
        core.add_playlist(imported_id, vec![1]).unwrap();
        assert!(core.state.library.playlist_entry_revisions[&imported_id] > imported_revision);
        assert_eq!(
            core.state.library.playlist_entry_revisions[&existing_id],
            scopes[&existing_id]
        );
    }
}

fn save_prepared_scan(core: &mut Core, scan: ScanFinished) -> ScanTaskStatus {
    assert!(core.active_scan.is_none());
    core.active_scan = Some(ScanTask {
        status: ScanTaskStatus {
            id: scan.id,
            progress: ScanProgress {
                phase: ScanPhase::ReadingMetadata,
                ..scan.progress.clone()
            },
            message: String::new(),
            library_committed: false,
            playlist_id: None,
            error: None,
        },
        owner: if scan.import.is_some() {
            crate::core::state::MediaTaskOwner::Playlist
        } else {
            crate::core::state::MediaTaskOwner::Library
        },
        cancellation: library::ScanCancellation::default(),
        worker: core.executor.spawn(async {}),
        pending_save: None,
    });
    core.scan_event(ScanEvent::Finished(scan));
    settle_scan(core);
    core.scan_results.back().unwrap().clone()
}

#[test]
fn duration_threshold_is_snapshotted_for_file_directory_and_forced_scans() {
    for directory_scope in [false, true] {
        for force in [false, true] {
            for threshold in [0, 1] {
                let (directory, mut core) = fixture();
                let existing = directory.path().join("1.wav");
                let new = directory.path().join("new.wav");
                write_pcm(&existing);
                write_pcm(&new);
                Arc::make_mut(&mut core.state.configuration.config).scan_min_duration_secs =
                    threshold;
                let paths = if directory_scope {
                    vec![directory.path().to_path_buf()]
                } else {
                    vec![existing.clone(), new.clone()]
                };
                core.scan(paths, force).unwrap();
                // Mutating the live configuration after acceptance must not alter
                // this task, regardless of when its worker reaches the source.
                Arc::make_mut(&mut core.state.configuration.config).scan_min_duration_secs =
                    1 - threshold;
                assert!(settle_scan(&mut core));
                let status = core.scan_results.back().unwrap();
                assert_eq!(status.progress.errors, 0);
                assert!(status.error.is_none());
                assert_eq!(
                    core.store
                        .track_id_for_identity(crate::store::TrackIdentity::File(&existing))
                        .unwrap(),
                    Some(1)
                );
                let refreshed = core.store.track(1).unwrap().unwrap();
                assert!(!refreshed.missing);
                assert!(
                    refreshed
                        .duration
                        .is_some_and(|duration| duration > 0.0 && duration < 1.0)
                );
                assert_ne!(refreshed.title, "Track 1");
                assert_eq!(
                    core.store
                        .track_id_for_identity(crate::store::TrackIdentity::File(&new))
                        .unwrap()
                        .is_some(),
                    threshold == 0
                );
            }
        }
    }
}

#[test]
fn duration_only_exclusions_complete_successfully_without_errors() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("new.wav");
    write_pcm(&path);
    Arc::make_mut(&mut core.state.configuration.config).scan_min_duration_secs = 1;
    core.scan(vec![path.clone()], false).unwrap();
    assert!(settle_scan(&mut core));
    let task = core.scan_results.back().unwrap();
    assert_eq!(task.progress.errors, 0);
    assert!(task.error.is_none());
    assert!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))
            .unwrap()
            .is_none()
    );
}

#[test]
fn forced_duration_scan_refreshes_cached_short_existing_tracks() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    core.scan(vec![path.clone()], false).unwrap();
    assert!(settle_scan(&mut core));
    let known = core.store.known_source(&path).unwrap().unwrap();
    let mut media = known.media.unwrap();
    media.title = "Cached title".into();
    media.duration = Some(99.0);
    core.store
        .apply_scan(&ScanResult {
            records: (vec![ScanRecord {
                path: path.clone(),
                size: known.size,
                modified_ns: known.modified_ns,
                fingerprint: known.fingerprint,
                media,
            }])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    Arc::make_mut(&mut core.state.configuration.config).scan_min_duration_secs = 1;
    core.scan(vec![path], true).unwrap();
    assert!(settle_scan(&mut core));
    let track = core.store.track(1).unwrap().unwrap();
    assert!(!track.missing);
    assert_ne!(track.title, "Cached title");
    assert!(
        track
            .duration
            .is_some_and(|duration| duration > 0.0 && duration < 1.0)
    );
}

#[test]
fn duration_threshold_playlist_import_keeps_existing_tracks_and_uses_accepted_snapshot() {
    for extension in ["m3u8", "cue"] {
        let (directory, mut core) = fixture();
        let existing = directory.path().join("1.wav");
        let new = directory.path().join("new.wav");
        write_pcm_frames(&existing, 150);
        write_pcm_frames(&new, 150);
        let path = directory.path().join(format!("mix.{extension}"));
        if extension == "cue" {
            // Seed only the first segment, then introduce another short segment.
            std::fs::write(
                &path,
                "FILE \"new.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
            )
            .unwrap();
            core.scan(vec![path.clone()], false).unwrap();
            assert!(settle_scan(&mut core));
            std::fs::write(&path,
                "TITLE \"Imported album\"\nFILE \"new.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Updated segment\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 00:01:00\n").unwrap();
        } else {
            std::fs::write(&path, "1.wav\nnew.wav\n").unwrap();
        }
        let expected = if extension == "cue" {
            core.store
                .track_id_for_identity(crate::store::TrackIdentity::Cue(&path, 1))
                .unwrap()
                .unwrap()
        } else {
            1
        };
        Arc::make_mut(&mut core.state.configuration.config).scan_min_duration_secs = 3;
        core.command(Command::ImportPlaylist {
            path: path.clone(),
            name: Some("Filtered mix".into()),
        })
        .unwrap();
        Arc::make_mut(&mut core.state.configuration.config).scan_min_duration_secs = 0;
        assert!(settle_scan(&mut core));
        let task = core.scan_results.back().unwrap();
        assert_eq!(task.progress.errors, 0);
        assert!(task.error.is_none());
        let playlist = core.store.playlist_summary_page(None, 0, 10).unwrap().rows[0].id;
        let second = if extension == "cue" {
            Some(
                core.store
                    .track_id_for_identity(crate::store::TrackIdentity::Cue(&path, 2))
                    .unwrap()
                    .unwrap(),
            )
        } else {
            None
        };
        let expected_ids = second.map_or_else(|| vec![expected], |id| vec![expected, id]);
        assert_eq!(
            core.store.playlist_track_ids(playlist, 10).unwrap(),
            expected_ids
        );
        let track = core.store.track(expected).unwrap().unwrap();
        assert!(!track.missing);
        if extension == "cue" {
            assert_eq!(track.duration, Some(1.0));
            assert_eq!(track.title, "Updated segment");
        } else {
            assert_eq!(track.duration, Some(2.0));
        }
        assert_eq!(
            core.store
                .track_id_for_identity(crate::store::TrackIdentity::File(&new))
                .unwrap()
                .is_some(),
            extension == "cue"
        );
    }
}

#[test]
fn scan_start_ack_binds_the_accepted_task() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    let (requests, receiver) = async_channel::bounded(3);
    let (reply, ack) = oneshot::channel();
    requests
        .send_blocking(Request {
            operation: Operation::Command(Command::Scan {
                paths: vec![path],
                force: false,
            }),
            reply: Some(reply),
        })
        .unwrap();
    drop(requests);
    core = run_until_disconnected(core, receiver);
    let CoreResponse::Ack(ack) = futures::executor::block_on(ack).unwrap() else {
        panic!("Expected ack");
    };
    assert!(ack.is_success());
    let id = ack
        .scan_id
        .expect("Accepted scans must expose their identity");
    let ViewResponse::ScanTask(task) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("Expected task");
    };
    assert_eq!(task.id, id);
}

#[test]
fn rejected_library_scan_ack_preserves_unrelated_global_errors() {
    let (_directory, mut core) = fixture();
    core.state.system.last_error = Some("Unrelated audio failure".into());
    let (requests, receiver) = async_channel::bounded(1);
    let (reply, ack) = oneshot::channel();
    requests
        .send_blocking(Request {
            operation: Operation::Command(Command::Scan {
                paths: Vec::new(),
                force: false,
            }),
            reply: Some(reply),
        })
        .unwrap();
    drop(requests);
    core = run_until_disconnected(core, receiver);
    let CoreResponse::Ack(ack) = futures::executor::block_on(ack).unwrap() else {
        panic!("Expected ack");
    };
    assert!(!ack.is_success());
    assert!(
        ack.error
            .unwrap()
            .contains("No files or directories supplied")
    );
    assert!(ack.scan_id.is_none());
    assert!(core.scan_results.is_empty());
    assert_eq!(
        core.state.system.last_error.as_deref(),
        Some("Unrelated audio failure")
    );
}

#[test]
fn library_scan_failures_stay_in_their_result_while_import_errors_remain_global() {
    use crate::{core::state::MediaTaskOwner, projection::GuiSnapshot};
    for import in [false, true] {
        let (_directory, mut core) = fixture();
        core.state.system.last_error = Some("Unrelated audio failure".into());
        let status = save_prepared_scan(
            &mut core,
            ScanFinished {
                other_tracks_check: false,
                id: 1,
                result: Err(anyhow::anyhow!("Unreadable scan source")),
                import: import.then(|| ("Import".into(), Vec::new())),
                progress: ScanProgress {
                    phase: ScanPhase::Discovering,
                    processed: 0,
                    total: None,
                    errors: 0,
                    path: None,
                },
            },
        );
        assert_eq!(status.progress.phase, ScanPhase::Failed);
        assert_eq!(status.error.as_deref(), Some("Unreadable scan source"));
        core.publish();
        let snapshot = GuiSnapshot::from_core(&core.shared.read());
        assert_eq!(
            snapshot.system.scan_visible(MediaTaskOwner::Library),
            !import
        );
        assert_eq!(
            snapshot.system.scan_visible(MediaTaskOwner::Playlist),
            import
        );
        assert!(!snapshot.system.scan_visible(MediaTaskOwner::Queue));
        assert_eq!(
            snapshot.system.last_error.as_deref(),
            Some(if import {
                "Unreadable scan source"
            } else {
                "Unrelated audio failure"
            })
        );
        if !import {
            assert_eq!(snapshot.system.scan_message, "Unreadable scan source");
        }
    }
}

#[test]
fn completed_scan_wait_is_independent_of_later_scans_and_global_errors() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    core.scan(vec![path.clone()], false).unwrap();
    let id = core.active_scan.as_ref().unwrap().status.id;
    assert!(settle_scan(&mut core));
    let ViewResponse::ScanTask(before) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("Expected task");
    };
    core.state.system.last_error = Some("Unrelated audio failure".into());
    core.scan(vec![path], false).unwrap();
    let ViewResponse::ScanTask(after) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("Expected task");
    };
    assert_eq!(before, after);
    assert_ne!(core.active_scan.as_ref().unwrap().status.id, id);
    assert!(settle_scan(&mut core));
}

#[test]
fn verification_only_scan_invalidates_member_facts_not_display_or_playlist_pages() {
    let (_directory, mut core) = fixture();
    let path = core.track(1).unwrap().path;
    let known = core.store.known_source(&path).unwrap().unwrap();
    let mut record = ScanRecord {
        path,
        size: known.size,
        modified_ns: known.modified_ns,
        fingerprint: Some(blake3::hash(b"same encoded audio").to_hex().to_string()),
        media: known.media.unwrap(),
    };
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records: (vec![record.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..Default::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    core.command(Command::CreatePlaylistWithTracks {
        name: "Retained".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let track = core.track(1).unwrap();
    let history = Arc::clone(&core.state.library.history);
    let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
    let before = core.state.query_revisions();
    record.modified_ns += 1;
    let fingerprint = record.fingerprint.clone().unwrap();
    let status = save_prepared_scan(
        &mut core,
        ScanFinished {
            other_tracks_check: false,
            id: 1,
            result: Ok(ScanResult {
                records: (vec![record])
                    .into_iter()
                    .map(std::sync::Arc::new)
                    .collect(),
                ..Default::default()
            }),
            import: None,
            progress: ScanProgress {
                phase: ScanPhase::ReadingMetadata,
                processed: 1,
                total: Some(1),
                errors: 0,
                path: None,
            },
        },
    );
    assert_eq!(status.progress.phase, ScanPhase::Completed);
    let after = core.state.query_revisions();
    assert_eq!(
        (
            after.library,
            after.structure,
            after.playlist,
            after.queue,
            after.duplicate_groups
        ),
        (
            before.library,
            before.structure,
            before.playlist,
            before.queue,
            before.duplicate_groups
        )
    );
    assert_ne!(after.duplicate_members, before.duplicate_members);
    assert_eq!(core.track(1).unwrap(), track);
    assert!(Arc::ptr_eq(&history, &core.state.library.history));
    assert!(Arc::ptr_eq(
        &scopes,
        &core.state.library.playlist_entry_revisions
    ));
    let query = Query::DuplicateMembers {
        fingerprint,
        offset: 0,
        limit: 10,
    };
    assert_ne!(query.stamp(after), query.stamp(before));
    let QueryResponse {
        result: Ok(ViewResponse::DuplicateMembers(members)),
        ..
    } = core.query_response(&query)
    else {
        panic!("expected current member facts");
    };
    assert_eq!(members.rows[0].modified_ns, known.modified_ns + 1);
}

#[test]
fn scan_lookup_wait_observes_cancel_and_shutdown_without_committing() {
    for shutdown in [false, true] {
        let (directory, mut core) = fixture();
        let path = directory.path().join("new.wav");
        write_pcm(&path);
        let total = core.state.library.track_total;
        core.scan(vec![path], false).unwrap();
        let id = core.active_scan.as_ref().unwrap().status.id;
        let lookup = loop {
            let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if matches!(event, ScanEvent::KnownSources { .. }) {
                break event;
            }
            core.scan_event(event);
        };
        // Keep the request alive without replying: cancellation must wake the
        // worker even when owner disconnection cannot release its reply wait.
        if shutdown {
            core.shutdown_scan();
        } else {
            core.cancel_scan(id).unwrap();
            assert!(!settle_scan(&mut core));
        }
        drop(lookup);
        assert!(core.active_scan.is_none());
        assert_eq!(core.state.library.track_total, total);
        assert_eq!(
            core.scan_results.back().unwrap().progress.phase,
            ScanPhase::Cancelled
        );
    }
}

#[test]
fn cancelling_finished_but_uncommitted_scan_discards_its_result() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("new.wav");
    write_pcm(&path);
    let total = core.state.library.track_total;
    core.scan(vec![path], false).unwrap();
    let id = core.active_scan.as_ref().unwrap().status.id;
    let finished = loop {
        match core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            ScanEvent::Progress { .. } => {}
            event @ ScanEvent::KnownSources { .. } => core.scan_event(event),
            ScanEvent::Finished(scan) => break scan,
            ScanEvent::Saved(_) => panic!("Scan cannot save before Core authorizes it"),
        }
    };
    core.command(Command::CancelScan { id }).unwrap();
    core.scan_event(ScanEvent::Finished(finished));
    assert_eq!(core.state.library.track_total, total);
    assert_eq!(
        core.scan_results.back().unwrap().progress.phase,
        ScanPhase::Cancelled
    );
    assert!(core.active_scan.is_none());
    assert!(!core.scan_results.back().unwrap().library_committed);
    assert_eq!(core.scan_results.back().unwrap().playlist_id, None);
}

#[test]
fn saving_scan_rejects_cancellation_and_invalidates_the_owner_cache() {
    let (directory, mut core) = fixture();
    let cached = core.track(1).unwrap();
    let source = cached.path.clone();
    write_pcm(&source);
    core.scan(vec![source], true).unwrap();
    let finished = loop {
        match core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            ScanEvent::Progress { .. } => {}
            event @ ScanEvent::KnownSources { .. } => core.scan_event(event),
            ScanEvent::Finished(scan) => break scan,
            ScanEvent::Saved(_) => panic!("Saving requires Core authorization"),
        }
    };
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    core.scan_event(ScanEvent::Finished(finished));
    let status = &core.active_scan.as_ref().unwrap().status;
    let id = status.id;
    assert_eq!(status.progress.phase, ScanPhase::Saving);
    assert!(!status.library_committed);
    assert!(core.database_maintenance_active());
    assert_eq!(
        core.shared
            .read()
            .system
            .scan_progress
            .as_ref()
            .unwrap()
            .phase,
        ScanPhase::Saving
    );
    let error = core.cancel_scan(id).unwrap_err();
    assert!(error.to_string().contains("can no longer be cancelled"));
    assert!(
        !core
            .active_scan
            .as_ref()
            .unwrap()
            .cancellation
            .is_cancelled()
    );
    connection.execute_batch("ROLLBACK").unwrap();
    assert!(settle_scan(&mut core));
    assert!(!core.database_maintenance_active());
    assert!(core.scan_results.back().unwrap().library_committed);
    let refreshed = core.track(1).unwrap();
    assert_ne!(refreshed.title, cached.title);
    let observer = Store::open(&directory.path().join("library.db")).unwrap();
    assert_eq!(refreshed, observer.track(1).unwrap().unwrap());
}
#[test]
fn stale_scan_events_cannot_replace_the_active_task() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    core.scan(vec![path], false).unwrap();
    let before = core.active_scan.as_ref().unwrap().status.clone();
    let progress = ScanProgress {
        phase: ScanPhase::Failed,
        processed: 999,
        total: None,
        errors: 1,
        path: None,
    };
    core.scan_event(ScanEvent::Progress {
        id: 0,
        progress: progress.clone(),
    });
    core.scan_event(ScanEvent::Finished(ScanFinished {
        other_tracks_check: false,
        id: 0,
        result: Ok(ScanResult::default()),
        import: None,
        progress,
    }));
    let (reply, receive) = oneshot::channel();
    core.scan_event(ScanEvent::KnownSources {
        id: 0,
        paths: vec![directory.path().join("1.wav")],
        reply,
    });
    assert!(futures::executor::block_on(receive).unwrap().is_err());
    assert_eq!(core.active_scan.as_ref().unwrap().status, before);
    assert!(core.scan_results.is_empty());
    assert!(settle_scan(&mut core));
}

#[test]
fn scan_terminal_retention_is_bounded_and_unknown_ids_fail() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    for _ in 0..MAX_RETAINED_SCAN_RESULTS + 1 {
        core.scan(vec![path.clone()], false).unwrap();
        assert!(settle_scan(&mut core));
    }
    assert_eq!(core.scan_results.len(), MAX_RETAINED_SCAN_RESULTS);
    assert!(core.view(&Query::ScanTask { id: 1 }).is_err());
    assert!(core.view(&Query::ScanTask { id: u64::MAX }).is_err());
    assert!(
        core.view(&Query::ScanTask {
            id: MAX_RETAINED_SCAN_RESULTS as u64 + 1
        })
        .is_ok()
    );
}

#[test]
fn lowering_scan_depth_preserves_excluded_tracks_until_they_are_scanned() {
    let (directory, mut core) = fixture();
    let allowed = directory.path().join("allowed");
    let excluded = allowed.join("excluded");
    std::fs::create_dir_all(&excluded).unwrap();
    let visible = allowed.join("visible.wav");
    let hidden = excluded.join("hidden.wav");
    write_pcm(&visible);
    write_pcm(&hidden);
    let mut config = core.state.configuration.config.as_ref().clone();
    config.scan_max_depth = 2;
    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap();
    core.command(Command::Scan {
        paths: vec![directory.path().to_path_buf()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let hidden_id = core
        .store
        .track_id_for_identity(crate::store::TrackIdentity::File(&hidden))
        .unwrap()
        .unwrap();
    core.store.set_favorite(&[hidden_id], true).unwrap();
    core.store
        .edit_track(
            hidden_id,
            Some("Kept title"),
            Some("Artist"),
            Some("Album"),
            None,
        )
        .unwrap();
    std::fs::remove_file(&hidden).unwrap();
    let mut config = core.state.configuration.config.as_ref().clone();
    config.scan_max_depth = 1;
    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap();
    core.command(Command::Scan {
        paths: vec![directory.path().to_path_buf()],
        force: false,
    })
    .unwrap();
    assert!(!settle_scan(&mut core));
    assert!(
        core.scan_results
            .back()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains(excluded.to_str().unwrap())
    );
    let preserved = core.track(hidden_id).unwrap();
    assert!(!preserved.missing);
    assert!(preserved.favorite);
    assert_eq!(preserved.title, "Kept title");
    let visible_id = core
        .store
        .track_id_for_identity(crate::store::TrackIdentity::File(&visible))
        .unwrap()
        .unwrap();
    assert!(!core.track(visible_id).unwrap().missing);
    let mut config = core.state.configuration.config.as_ref().clone();
    config.scan_max_depth = 2;
    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap();
    core.command(Command::Scan {
        paths: vec![directory.path().to_path_buf()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let removed = core.track(hidden_id).unwrap();
    assert!(removed.missing);
    assert!(removed.favorite);
    assert_eq!(removed.title, "Kept title");
}

#[test]
fn scan_rejects_when_no_library_roots_are_configured() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.configuration.config)
        .library_roots
        .clear();
    assert!(
        core.command(Command::Scan {
            paths: vec![directory.path().to_path_buf()],
            force: false,
        })
        .is_err()
    );
    assert!(!core.state.system.scanning);
    assert!(core.active_scan.is_none());
}

#[test]
fn scan_accepts_a_child_of_a_configured_root() {
    let (directory, mut core) = fixture();
    let child = directory.path().join("nested");
    std::fs::create_dir(&child).unwrap();
    core.command(Command::Scan {
        paths: vec![child],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    assert_eq!(core.store.library_stats().unwrap().total, 4);
}

#[test]
fn scan_rejects_prefix_sibling_of_a_configured_root() {
    let (_directory, mut core) = fixture();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("Music");
    let sibling = parent.path().join("Music-other");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    Arc::make_mut(&mut core.state.configuration.config).library_roots = vec![root];
    assert!(
        core.command(Command::Scan {
            paths: vec![sibling],
            force: false,
        })
        .is_err()
    );
    assert!(core.active_scan.is_none());
}

#[test]
fn scan_accepts_symlinks_to_outside_a_configured_root() {
    let (directory, mut core) = fixture();
    let outside = tempfile::tempdir().unwrap();
    let source = outside.path().join("source.wav");
    write_pcm(&source);
    let link = directory.path().join("outside-link");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let logical = link.join("source.wav");
    core.command(Command::Scan {
        paths: vec![link.clone()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let id = core
        .store
        .track_id_for_identity(crate::store::TrackIdentity::File(&logical))
        .unwrap()
        .unwrap();
    assert_eq!(core.track(id).unwrap().path, logical);
    assert!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&source))
            .unwrap()
            .is_none()
    );
    let page = core
        .store
        .directory_page(&link, None, None, None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(page.total, 1);
    assert!(matches!(&page.rows[0], DirectoryRow::Track(row) if row.id == id));
}

#[test]
fn scan_keeps_distinct_configured_aliases_of_the_same_directory() {
    let (directory, mut core) = fixture();
    let outside = tempfile::tempdir().unwrap();
    write_pcm(&outside.path().join("source.wav"));
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    for alias in [&first, &second] {
        std::os::unix::fs::symlink(outside.path(), alias).unwrap();
    }
    let mut config = core.state.configuration.config.as_ref().clone();
    config.library_roots = vec![first.join("."), second.join("unused/..")];
    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap();
    core.command(Command::Scan {
        paths: vec![first.join("unused/.."), second.join(".")],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let ids = [&first, &second].map(|alias| {
        let path = alias.join("source.wav");
        let id = core
            .store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))
            .unwrap()
            .unwrap();
        assert_eq!(core.track(id).unwrap().path, path);
        assert_eq!(
            core.store
                .directory_page(alias, None, None, None, 0, MAX_QUERY_ROWS)
                .unwrap()
                .total,
            1
        );
        id
    });
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn scan_of_one_root_ignores_an_unavailable_configured_root() {
    let (directory, mut core) = fixture();
    let source = directory.path().join("1.wav");
    write_pcm(&source);
    let duration_before = core.track(1).unwrap().duration;
    Arc::make_mut(&mut core.state.configuration.config)
        .library_roots
        .push(directory.path().join("offline/Music"));
    core.command(Command::Scan {
        paths: vec![source.clone()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let track = core.track(1).unwrap();
    assert_eq!(track.path, source);
    assert!(!track.missing);
    assert_ne!(track.duration, duration_before);
    assert!(
        track
            .duration
            .is_some_and(|duration| duration > 0.0 && duration < 1.0)
    );
}

#[test]
fn cue_scan_can_reference_another_configured_root() {
    let (directory, mut core) = fixture();
    let sheets = directory.path().join("sheets");
    let sources = directory.path().join("sources");
    std::fs::create_dir(&sheets).unwrap();
    std::fs::create_dir(&sources).unwrap();
    let source = sources.join("source.wav");
    write_pcm(&source);
    let sheet = sheets.join("album.cue");
    std::fs::write(
        &sheet,
        "FILE ../sources/source.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
    )
    .unwrap();
    Arc::make_mut(&mut core.state.configuration.config).library_roots = vec![sheets, sources];
    core.command(Command::Scan {
        paths: vec![sheet.clone()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let id = core
        .store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(&sheet, 1))
        .unwrap()
        .unwrap();
    let track = core.track(id).unwrap();
    assert_eq!(track.path, source);
    assert_eq!(track.cue.as_ref().unwrap().sheet, sheet);
}

#[test]
fn scan_rejects_a_logical_parent_escape_even_when_the_path_exists() {
    let (_directory, mut core) = fixture();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("Music");
    std::fs::create_dir(&root).unwrap();
    Arc::make_mut(&mut core.state.configuration.config).library_roots = vec![root.clone()];
    assert!(
        core.command(Command::Scan {
            paths: vec![root.join("..")],
            force: false,
        })
        .is_err()
    );
    assert!(core.active_scan.is_none());
}

#[test]
fn missing_and_broken_requests_do_not_prevent_other_roots_from_scanning() {
    let (directory, mut core) = fixture();
    let outside = tempfile::tempdir().unwrap();
    let missing = outside.path().join("offline");
    let broken = directory.path().join("broken-link");
    std::os::unix::fs::symlink(&missing, &broken).unwrap();
    let source = directory.path().join("1.wav");
    write_pcm(&source);
    Arc::make_mut(&mut core.state.configuration.config)
        .library_roots
        .push(missing.clone());
    core.command(Command::Scan {
        paths: vec![missing, broken, source.clone()],
        force: false,
    })
    .unwrap();
    assert!(!settle_scan(&mut core));
    assert!(core.scan_results.back().unwrap().error.is_some());
    assert!(core.state.system.last_error.is_none());
    let track = core.track(1).unwrap();
    assert_eq!(track.path, source);
    assert!(!track.missing);
    assert!(track.duration.unwrap() < 1.0);
}

#[test]
fn playlist_import_and_directory_queries_preserve_configured_aliases() {
    let (directory, mut core) = fixture();
    let outside = tempfile::tempdir().unwrap();
    let source = outside.path().join("source.wav");
    write_pcm(&source);
    std::fs::write(
        outside.path().join("playlist.m3u"),
        "./unused/../source.wav\n",
    )
    .unwrap();
    let alias = directory.path().join("Music");
    std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
    let mut config = core.state.configuration.config.as_ref().clone();
    config.library_roots = vec![alias.join("unused/.."), outside.path().to_path_buf()];
    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap();
    assert_eq!(
        core.state.configuration.config.library_roots,
        [alias.clone(), outside.path().to_path_buf()]
    );
    core.command(Command::Scan {
        paths: vec![source.clone()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let physical_id = core
        .store
        .track_id_for_identity(crate::store::TrackIdentity::File(&source))
        .unwrap()
        .unwrap();
    core.command(Command::ImportPlaylist {
        path: alias.join("playlist.m3u"),
        name: Some("Alias playlist".into()),
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let logical = alias.join("source.wav");
    let logical_id = core
        .store
        .track_id_for_identity(crate::store::TrackIdentity::File(&logical))
        .unwrap()
        .unwrap();
    assert_ne!(logical_id, physical_id);
    let playlist_id = core.store.playlist_summary_page(None, 0, 1).unwrap().rows[0].id;
    let entries = core
        .store
        .playlist_entries_page(playlist_id, None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(entries.rows.len(), 1);
    assert_eq!(entries.rows[0].track_id, logical_id);
    let page = core
        .store
        .directory_page(
            &alias.join("unused/.."),
            None,
            None,
            None,
            0,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(page.total, 1);
    assert!(matches!(&page.rows[0], DirectoryRow::Track(row) if row.id == logical_id));
    core.enqueue(&[logical_id]).unwrap();
    let entries = core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap().rows;
    let entry = entries.last().unwrap();
    assert_eq!(entry.track_id, logical_id);
    assert_eq!(core.track(entry.track_id).unwrap().path, logical);
}

#[test]
fn mixed_inside_and_outside_scan_is_rejected_atomically() {
    let (directory, mut core) = fixture();
    let inside = directory.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let total = core.state.library.track_total;
    assert!(
        core.command(Command::Scan {
            paths: vec![inside, outside.path().to_path_buf()],
            force: false,
        })
        .is_err()
    );
    assert_eq!(core.state.library.track_total, total);
    assert!(!core.state.system.scanning);
    assert!(core.active_scan.is_none());
}

#[test]
fn committed_impacts_refresh_current_track_activity_and_probe_metadata() {
    let (directory, mut core) = fixture();
    core.store.set_queue_current(Some(1)).unwrap();
    core.refresh_queue_tracks().unwrap();
    core.store.mark_played(1, 7).unwrap();
    core.apply_scan_impacts(crate::store::ScanImpacts {
        changed: true,
        library: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(core.state.current_track().unwrap().last_played, Some(7));

    let known = core
        .store
        .stored_scan_files(&[directory.path().to_path_buf()])
        .unwrap()
        .into_iter()
        .find(|file| file.track_id == 1)
        .unwrap();
    let mut media = known.media.unwrap();
    media.codec = "updated-codec".into();
    media.sample_rate = 96_000;
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            roots: vec![directory.path().to_path_buf()],
            min_duration_secs: 0,
            skipped_directories: Vec::new(),
            skipped_directories_truncated: false,
            scan_incomplete: true,
            uncertainties: Vec::new(),
            records: (vec![ScanRecord {
                path: known.path,
                size: known.size,
                modified_ns: known.modified_ns + 1,
                fingerprint: known.fingerprint,
                media,
            }])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
            errors: Vec::new(),
            ..Default::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    let current = core.state.current_track().unwrap();
    assert_eq!(current.codec, "updated-codec");
    assert_eq!(current.sample_rate, 96_000);
}

#[test]
fn database_optimization_requires_stopped_playback_and_no_scan() {
    let (_directory, mut core) = fixture_with_database(Some(10.0));
    playing(&mut core, 1);
    assert!(core.command(Command::OptimizeDatabase).is_err());
    core.command(Command::Pause).unwrap();
    assert!(core.command(Command::OptimizeDatabase).is_err());
    core.stop().unwrap();
    core.state.system.scanning = true;
    assert!(core.command(Command::OptimizeDatabase).is_err());
    core.state.system.scanning = false;
    core.enqueue(&[4]).unwrap();
    core.command(Command::OptimizeDatabase).unwrap();
    settle_optimization(&mut core).unwrap();
    assert!(core.state.system.database_optimization.is_some());
    assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
    assert_eq!(play_count(&core, 1), 0);
}

#[test]
fn forced_scan_reprobes_unchanged_files_instead_of_reusing_cached_metadata() {
    let (_directory, mut core) = fixture();
    let path = core.track(1).unwrap().path.clone();
    write_pcm(&path);
    let mut scanned = library::scan_paths(
        std::slice::from_ref(&path),
        |paths| Ok(vec![None; paths.len()]),
        false,
        &[_directory.path().to_path_buf()],
        core.state.configuration.config.scan_max_depth,
        &library::ScanCancellation::default(),
        |_, _, _, _, _| {},
    )
    .unwrap();
    let probed_title = scanned.records[0].media.title.clone();
    std::sync::Arc::make_mut(&mut scanned.records[0])
        .media
        .title = "Cached metadata".into();
    let impacts = core.store.apply_scan(&scanned).unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    for (force, expected) in [(false, "Cached metadata"), (true, probed_title.as_str())] {
        core.command(Command::Scan {
            paths: vec![path.clone()],
            force,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        assert_eq!(core.track(1).unwrap().title, expected);
    }
}

#[test]
fn m3u_playlist_and_queue_preserve_occurrence_order() {
    let (directory, mut core) = fixture();
    for track_id in [1, 2, 3] {
        write_pcm(&core.track(track_id).unwrap().path);
    }
    let path = directory.path().join("duplicates.m3u");
    std::fs::write(&path, "2.wav\n1.wav\n2.wav\n3.wav\n1.wav\n").unwrap();
    core.command(Command::ImportPlaylist {
        path,
        name: Some("Repeated occurrences".into()),
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let playlist_id = core.store.playlist_summary_page(None, 0, 1).unwrap().rows[0].id;
    assert_eq!(
        core.store
            .playlist_entries_page(playlist_id, None, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [2, 1, 2, 3, 1]
    );
    core.command(Command::Enqueue {
        track_ids: vec![2, 1, 2, 3, 1],
    })
    .unwrap();
    assert_eq!(
        core.store
            .queue_page(None, 5, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [2, 1, 2, 3, 1]
    );
}

#[test]
fn cue_playlist_import_preserves_repeated_segments_in_order() {
    let (directory, mut core) = fixture();
    let sheet = directory.path().join("album.cue");
    std::fs::write(&sheet, "FILE album.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:05:00\n").unwrap();
    let items = library::import_playlist(&sheet).unwrap();
    let source = Arc::new(ScanRecord {
        path: directory.path().join("album.wav"),
        size: 1,
        modified_ns: 1,
        fingerprint: None,
        media: MediaInfo {
            title: "Album source".into(),
            artist: String::new(),
            album: "Album".into(),
            duration: Some(10.0),
            codec: "pcm".into(),
            channels: 2,
            sample_rate: 48_000,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
        },
    });
    let segments = (1..=2)
        .map(|number| {
            library::CueRecord::registered(
                Arc::clone(&source),
                CueSegment {
                    sheet: sheet.clone(),
                    number,
                    start_frame: u64::from(number - 1) * 375,
                    end_frame: if number == 1 { Some(375) } else { None },
                },
                library::RegisteredCueMetadata {
                    title: format!("Part {number}"),
                    artist: String::new(),
                    album: "Album".into(),
                    track_number: Some(number),
                    disc_number: None,
                    release_date: None,
                },
                Some(5.0),
            )
        })
        .collect();
    let status = save_prepared_scan(
        &mut core,
        ScanFinished {
            other_tracks_check: false,
            id: 0,
            result: Ok(ScanResult {
                roots: vec![sheet],
                segments,
                ..ScanResult::default()
            }),
            import: Some((
                "CUE occurrences".into(),
                vec![items[1].clone(), items[0].clone(), items[1].clone()],
            )),
            progress: ScanProgress {
                phase: ScanPhase::ReadingMetadata,
                processed: 1,
                total: Some(1),
                errors: 0,
                path: None,
            },
        },
    );
    assert_eq!(status.progress.phase, ScanPhase::Completed);
    let playlist_id = core.store.playlist_summary_page(None, 0, 1).unwrap().rows[0].id;
    let entries = core
        .store
        .playlist_entries_page(playlist_id, None, 0, MAX_QUERY_ROWS)
        .unwrap()
        .rows;
    let tracks: Vec<_> = entries
        .iter()
        .map(|entry| core.track(entry.track_id).unwrap())
        .collect();
    assert_eq!(
        tracks
            .iter()
            .map(|track| track.title.as_str())
            .collect::<Vec<_>>(),
        ["Part 2", "Part 1", "Part 2"]
    );
    assert_ne!(tracks[0].id, tracks[1].id);
    assert_ne!(entries[0].id, entries[1].id);
    assert_eq!(tracks[0].id, tracks[2].id);
    assert_ne!(entries[0].id, entries[2].id);
}

#[test]
fn import_input_failures_are_terminal_tasks_without_database_changes() {
    for kind in [
        "missing",
        "fifo",
        "nested_fifo",
        "oversized",
        "expansion",
        "invalid_utf8",
    ] {
        let (directory, mut core) = fixture();
        let path = directory.path().join("bounded.m3u");
        match kind {
            "missing" => {}
            "fifo" | "nested_fifo" => {
                let fifo = if kind == "fifo" {
                    path.clone()
                } else {
                    directory.path().join("nested.cue")
                };
                let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                if kind == "nested_fifo" {
                    std::fs::write(&path, "nested.cue\n").unwrap();
                }
            }
            "oversized" => {
                std::fs::File::create(&path)
                    .unwrap()
                    .set_len(32 * 1024 * 1024)
                    .unwrap();
            }
            "expansion" => {
                std::fs::write(directory.path().join("album.cue"),
                    "FILE album.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00\n").unwrap();
                std::fs::write(&path, "album.cue\n".repeat(50_001)).unwrap();
            }
            "invalid_utf8" => std::fs::write(&path, [0xff]).unwrap(),
            _ => unreachable!(),
        }
        // Check the failing boundary, not the full user-facing error chain.
        let expected_source = match kind {
            "missing" => std::fs::File::open(&path).unwrap_err().to_string(),
            "fifo" | "nested_fifo" => "not an ordinary file".into(),
            "oversized" => "input exceeds".into(),
            "expansion" => "import expansion exceeds".into(),
            "invalid_utf8" => "UTF-8".into(),
            _ => unreachable!(),
        };
        let before = core.state.query_revisions();
        core.command(Command::ImportPlaylist {
            path,
            name: Some("Bounded".into()),
        })
        .unwrap();
        assert!(core.active_scan.is_some());
        assert!(!settle_scan(&mut core));
        let status = core.scan_results.back().unwrap();
        assert_eq!(status.progress.phase, ScanPhase::Failed);
        let error = status.error.as_deref().unwrap();
        assert!(error.contains(&expected_source), "{kind}: {error}");
        assert_eq!(core.state.query_revisions(), before);
        assert_eq!(core.store.track_count().unwrap(), 4);
        assert_eq!(
            core.store.playlist_summary_page(None, 0, 0).unwrap().total,
            0
        );
    }
}

#[test]
fn accepted_import_allows_dispatch_and_cancellation_before_commit() {
    let (directory, mut core) = fixture();
    write_pcm(&directory.path().join("new.wav"));
    let path = directory.path().join("mix.m3u");
    std::fs::write(&path, "new.wav\n").unwrap();
    core.command(Command::ImportPlaylist { path, name: None })
        .unwrap();
    let id = core.active_scan.as_ref().unwrap().status.id;
    // Hold the worker at its existing owner lookup, without timing assumptions.
    let lookup = loop {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if matches!(&event, ScanEvent::KnownSources { .. }) {
            break event;
        }
        core.scan_event(event);
    };
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: true,
    })
    .unwrap();
    core.cancel_scan(id).unwrap();
    core.scan_event(lookup);
    assert!(!settle_scan(&mut core));
    assert_eq!(
        core.scan_results.back().unwrap().progress.phase,
        ScanPhase::Cancelled
    );
    assert_eq!(core.store.track_count().unwrap(), 4);
    assert_eq!(
        core.store.playlist_summary_page(None, 0, 0).unwrap().total,
        0
    );
}

fn settle_optimization(core: &mut Core) -> Result<()> {
    let finished = &mut core.active_optimization.as_mut().unwrap().finished;
    let result = futures::executor::block_on(async {
        rivu_protocol::transport::timeout(Duration::from_secs(15), finished)
            .await
            .unwrap()
            .unwrap()
    });
    let (reply, result) = core.finish_optimization(result);
    assert!(reply.is_none());
    assert!(core.active_optimization.is_none());
    result.map(|_| ())
}

#[test]
fn database_optimization_remains_single_flight_after_requester_disconnects() {
    let (_directory, mut core) = fixture_with_database(Some(10.0));
    core.command(Command::OptimizeDatabase).unwrap();
    let (reply, requester) = oneshot::channel();
    core.active_optimization.as_mut().unwrap().reply = Some(reply);
    drop(requester);
    assert!(core.command(Command::OptimizeDatabase).is_err());
    assert!(core.active_optimization.is_some());
    let finished = &mut core.active_optimization.as_mut().unwrap().finished;
    let result = futures::executor::block_on(async {
        rivu_protocol::transport::timeout(Duration::from_secs(15), finished)
            .await
            .unwrap()
            .unwrap()
    });
    // Even an available result does not retire the operation or replay it.
    assert!(core.command(Command::OptimizeDatabase).is_err());
    let (reply, result) = core.finish_optimization(result);
    result.unwrap();
    assert!(reply.is_some());
    assert!(core.active_optimization.is_none());
    assert!(core.state.system.database_optimization.is_some());
    core.command(Command::OptimizeDatabase).unwrap();
    settle_optimization(&mut core).unwrap();
}

#[test]
fn database_optimization_checkpoint_failure_retires_owner_and_allows_explicit_retry() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: true,
    })
    .unwrap();
    let queue = core.store.queue_ids().unwrap();
    let reader = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM tracks;")
        .unwrap();
    core.command(Command::OptimizeDatabase).unwrap();
    let error = settle_optimization(&mut core).unwrap_err();
    // This is a post-VACUUM failure, not evidence that the database was untouched.
    assert!(format!("{error:#}").contains("checkpoint"));
    assert!(core.state.system.database_optimization.is_none());
    assert_eq!(core.store.queue_ids().unwrap(), queue);
    assert!(core.store.track(1).unwrap().unwrap().favorite);
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    core.command(Command::OptimizeDatabase).unwrap();
    settle_optimization(&mut core).unwrap();
    let report = core.state.system.database_optimization.as_ref().unwrap();
    assert!(report.database_bytes_after > 0);
    assert_eq!(report.wal_bytes_after, 0);
    assert_eq!(core.store.queue_ids().unwrap(), queue);
    assert!(core.store.track(1).unwrap().unwrap().favorite);
}

#[test]
fn database_optimization_shutdown_waits_for_final_worker_result() {
    let (_directory, mut core) = fixture_with_database(Some(10.0));
    core.command(Command::OptimizeDatabase).unwrap();
    let (reply, receiver) = oneshot::channel();
    core.active_optimization.as_mut().unwrap().reply = Some(reply);
    let (reply, result) = core.shutdown_optimization().unwrap();
    assert!(core.active_optimization.is_none());
    let report = result.as_ref().unwrap().clone();
    core.complete_optimization(reply, result);
    let CoreResponse::Ack(ack) = futures::executor::block_on(receiver).unwrap() else {
        panic!("expected optimization acknowledgment");
    };
    assert!(ack.is_success());
    assert_eq!(ack.database_optimization, Some(report.clone()));
    assert_eq!(core.state.system.database_optimization, Some(report));
    assert!(core.shutdown_optimization().is_none());
}

#[test]
fn database_optimization_failed_ack_never_uses_a_previous_report() {
    let (_directory, mut core) = fixture_with_database(Some(10.0));
    core.command(Command::OptimizeDatabase).unwrap();
    settle_optimization(&mut core).unwrap();
    assert!(core.state.system.database_optimization.is_some());
    let (reply, receiver) = oneshot::channel();
    core.complete_optimization(Some(reply), Err(anyhow::anyhow!("optimization failed")));
    let CoreResponse::Ack(ack) = futures::executor::block_on(receiver).unwrap() else {
        panic!("expected optimization acknowledgment");
    };
    assert_eq!(ack.status, rivu_protocol::response::AckStatus::Failed);
    assert!(ack.database_optimization.is_none());
    assert!(core.state.system.database_optimization.is_some());
}

fn prepare_other_check(core: &mut Core, force: bool) -> ScanFinished {
    let id = core.start_other_tracks_check(force).unwrap();
    assert!(core.state.system.scanning);
    assert_eq!(core.state.system.scan_task_id, Some(id));
    loop {
        match core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            ScanEvent::Finished(finished) => return finished,
            event => core.scan_event(event),
        }
    }
}

#[test]
fn other_check_empty_roots_rechecks_only_registered_sources_without_duration_filter() {
    let (directory, mut core) = fixture();
    let existing = directory.path().join("1.wav");
    let unregistered = directory.path().join("unregistered.wav");
    write_pcm(&existing);
    write_pcm(&unregistered);
    let config = Arc::make_mut(&mut core.state.configuration.config);
    config.library_roots.clear();
    config.scan_min_duration_secs = 3600;
    core.command(Command::CheckOtherTracks { force: true })
        .unwrap();
    let id = core.active_scan.as_ref().unwrap().status.id;
    assert!(core.state.system.scanning);
    assert_eq!(core.state.system.scan_task_id, Some(id));
    assert!(settle_scan(&mut core));
    assert!(!core.state.system.scanning);
    assert_eq!(core.state.system.scan_task_id, Some(id));
    assert!(!core.store.track(1).unwrap().unwrap().missing);
    for id in 2..=4 {
        assert!(core.store.track(id).unwrap().unwrap().missing);
    }
    assert_eq!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&unregistered))
            .unwrap(),
        None
    );
    assert_eq!(core.store.track_count().unwrap(), 4);
    assert_eq!(core.scan_results.back().unwrap().progress.total, Some(4));
}

#[test]
fn other_check_rechecks_roots_after_preparation_and_before_saving() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.configuration.config)
        .library_roots
        .clear();
    let finished = prepare_other_check(&mut core, true);
    assert_eq!(finished.result.as_ref().unwrap().roots.len(), 4);
    Arc::make_mut(&mut core.state.configuration.config).library_roots =
        vec![directory.path().join("unused/..")];
    core.scan_event(ScanEvent::Finished(finished));
    assert!(settle_scan(&mut core));
    for id in 1..=4 {
        assert!(!core.store.track(id).unwrap().unwrap().missing);
    }
}

#[test]
fn other_check_cancel_and_shutdown_join_preparation_waiting_on_owner() {
    for shutdown in [false, true] {
        let (directory, mut core) = fixture();
        write_pcm(&directory.path().join("1.wav"));
        Arc::make_mut(&mut core.state.configuration.config)
            .library_roots
            .clear();
        let id = core.start_other_tracks_check(false).unwrap();
        let lookup = loop {
            let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if matches!(event, ScanEvent::KnownSources { .. }) {
                break event;
            }
            core.scan_event(event);
        };
        if shutdown {
            core.shutdown_scan();
        } else {
            core.cancel_scan(id).unwrap();
            assert!(!settle_scan(&mut core));
        }
        drop(lookup);
        assert!(core.active_scan.is_none());
        assert_eq!(
            core.scan_results.back().unwrap().progress.phase,
            ScanPhase::Cancelled
        );
        assert!(!core.scan_results.back().unwrap().library_committed);
        for track_id in 1..=4 {
            assert!(!core.store.track(track_id).unwrap().unwrap().missing);
        }
    }
}

#[test]
fn other_check_saving_rejects_cancel_and_shutdown_waits_for_commit() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.configuration.config)
        .library_roots
        .clear();
    let finished = prepare_other_check(&mut core, true);
    let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let id = finished.id;
    core.scan_event(ScanEvent::Finished(finished));
    assert_eq!(
        core.active_scan.as_ref().unwrap().status.progress.phase,
        ScanPhase::Saving
    );
    assert!(
        core.cancel_scan(id)
            .unwrap_err()
            .to_string()
            .contains("can no longer be cancelled")
    );
    lock.execute_batch("ROLLBACK").unwrap();
    core.shutdown_scan();
    assert!(core.active_scan.is_none());
    assert!(core.scan_results.back().unwrap().library_committed);
    assert_eq!(
        core.scan_results.back().unwrap().progress.phase,
        ScanPhase::Completed
    );
    for track_id in 1..=4 {
        assert!(core.store.track(track_id).unwrap().unwrap().missing);
    }
}
