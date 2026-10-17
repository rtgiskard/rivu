use super::*;
use crate::library::ScanRecord;
use std::sync::atomic::Ordering;

#[test]
fn runtime_rejects_non_utf8_config_paths_before_creating_storage() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    let invalid = OsString::from_vec(b"invalid-\xff".to_vec());
    let explicit_config = directory.path().join(&invalid).with_extension("toml");
    let derived_data_dir = directory.path().join(&invalid);
    for (data_dir, config_path) in [
        (directory.path().join("explicit-data"), explicit_config),
        (
            derived_data_dir.clone(),
            derived_data_dir.join("config.toml"),
        ),
    ] {
        let error = Runtime::start(&data_dir, &config_path).err().unwrap();
        assert!(
            error
                .to_string()
                .contains("Configuration path is not valid UTF-8")
        );
        assert!(!config_path.exists());
        assert!(!data_dir.exists());
    }
}

#[test]
fn runtime_validates_config_path_before_reading_or_rewriting_config() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join(OsString::from_vec(b"config-\xff.toml".to_vec()));
    let original = b"intentionally not valid TOML";
    std::fs::write(&path, original).unwrap();
    let error = Runtime::start(directory.path(), &path).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("Configuration path is not valid UTF-8")
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!directory.path().join("library.db").exists());
}

#[test]
fn startup_config_accepts_unicode_without_creating_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("配置.toml");
    assert_eq!(
        Runtime::load_startup_config(&path).unwrap(),
        Config::default()
    );
    assert!(!path.exists());
}

#[test]
fn startup_and_unchanged_exit_preserve_config_bytes_and_comments() {
    use std::os::unix::fs::MetadataExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let original = format!(
        "# Keep user comments and formatting\n[playback]\nvolume_percent = 75 # chosen volume\n[library]\nroots = [\"{}/unused/..\"]\n",
        directory.path().display(),
    ).into_bytes();
    std::fs::write(&path, &original).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let mut runtime = Runtime::start(directory.path(), &path).unwrap();
    assert_eq!(
        runtime.handle.config_snapshot().library_roots,
        [directory.path().to_path_buf()]
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    runtime.control.request_shutdown().unwrap();
    runtime.join().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
}

#[test]
fn missing_config_is_created_only_at_apply_or_orderly_exit() {
    for apply in [false, true] {
        let (_directory, mut core) = fixture();
        let _audio = recording_audio(&mut core);
        let path = core.state.system.config_path.clone();
        assert!(!path.exists());
        core.command(Command::Volume { value: 0.25 }).unwrap();
        core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
            .unwrap();
        core.publish();
        assert!(!path.exists());
        if apply {
            let baseline = core.state.system.config.clone();
            core.update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
                &baseline, &baseline,
            ))))
            .unwrap();
        } else {
            let (sender, receiver) = bounded(1);
            drop(sender);
            run_until_disconnected(&mut core, receiver);
        }
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.volume, 0.25);
        assert_eq!(
            saved.waveform_seek_enabled,
            core.state.system.config.waveform_seek_enabled
        );
    }
}

#[test]
fn runtime_start_with_missing_config_waits_until_exit_to_create_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut runtime = Runtime::start(directory.path(), &path).unwrap();
    assert!(!path.exists());
    runtime.control.request_shutdown().unwrap();
    runtime.join().unwrap();
    assert_eq!(Config::load(&path).unwrap(), Config::default());
    assert!(path.is_file());
}

#[test]
fn no_op_apply_and_change_back_exit_do_not_replace_saved_config() {
    use std::os::unix::fs::MetadataExt;

    let (_directory, mut core) = fixture();
    let audio = recording_audio(&mut core);
    core.flush_config().unwrap();
    let path = core.state.system.config_path.clone();
    let mut original = std::fs::read(&path).unwrap();
    original.extend_from_slice(b"\n# Preserve this comment on no-op saves\n");
    std::fs::write(&path, &original).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let baseline = core.state.system.config.clone();
    core.command(Command::Configure {
        config: baseline.clone(),
    })
    .unwrap();
    // Configure enqueues synchronously, but the recorder forwards on another thread.
    audio
        .recv_timeout(Duration::from_secs(2))
        .expect("No-op Apply still reapplies audio settings");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle {
        enabled: !baseline.shuffle,
    })
    .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(LibraryColumn::Artist))
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    core.command(Command::Volume {
        value: baseline.volume,
    })
    .unwrap();
    core.command(Command::Shuffle {
        enabled: baseline.shuffle,
    })
    .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(LibraryColumn::Artist))
        .unwrap();
    core.update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
        &baseline, &baseline,
    ))))
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    let (sender, receiver) = bounded(1);
    drop(sender);
    run_until_disconnected(&mut core, receiver);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
}

#[test]
fn queue_save_failure_does_not_prevent_final_config_save() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    let _audio = recording_audio(&mut core);
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.state.queue.current_id = Some(core.queue_ids[0]);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_final_queue_save BEFORE UPDATE ON queue_meta
         BEGIN SELECT RAISE(FAIL, 'queue save blocked'); END;",
        )
        .unwrap();
    let (sender, receiver) = bounded(1);
    drop(sender);
    run_until_disconnected(&mut core, receiver);
    assert!(
        core.shared
            .read()
            .system
            .last_error
            .as_deref()
            .unwrap()
            .contains("queue save blocked")
    );
    let saved = Config::load(&core.state.system.config_path).unwrap();
    assert_eq!(saved.volume, 0.25);
    assert_eq!(
        saved.waveform_seek_enabled,
        core.state.system.config.waveform_seek_enabled
    );
}

#[test]
fn runtime_completion_notifies_on_panic_and_reconciles_late_registration() {
    let (control, _shutdown) = RuntimeControl::new();
    let (wake, wakes) = bounded(1);
    control.set_completion_callback(move || {
        let _ = wake.try_send(());
    });
    let worker_control = control.clone();
    assert!(
        thread::spawn(move || {
            let _completion = CompletionGuard(worker_control);
            panic!("core worker failure");
        })
        .join()
        .is_err()
    );
    assert_eq!(wakes.try_recv(), Ok(()));
    let (late, late_wakes) = bounded(1);
    control.set_completion_callback(move || {
        let _ = late.try_send(());
    });
    assert_eq!(late_wakes.try_recv(), Ok(()));
    let ack = control.shutdown_ack(|| 17);
    assert!(!ack.ok);
    assert_eq!(ack.revision, 17);
}

#[test]
fn lifecycle_shutdown_coalesces_without_business_queue_capacity() {
    let (control, shutdown) = RuntimeControl::new();
    for _ in 0..1000 {
        control.request_shutdown().unwrap();
    }
    assert_eq!(shutdown.try_recv(), Ok(()));
    assert_eq!(
        shutdown.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    );
}

// An idle real audio worker handles stop/snapshot, but these tests never load
// audio. Missing-file errors expose the selected entry without opening output.
fn fixture() -> (tempfile::TempDir, Core) {
    fixture_with_duration(Some(10.0))
}

fn fixture_with_duration(duration: Option<f64>) -> (tempfile::TempDir, Core) {
    fixture_with_database(duration, false)
}

fn fixture_with_database(duration: Option<f64>, persistent: bool) -> (tempfile::TempDir, Core) {
    let directory = tempfile::tempdir().unwrap();
    let database_path = if persistent {
        directory.path().join("library.db")
    } else {
        Path::new(":memory:").to_path_buf()
    };
    let store = Store::open(&database_path).unwrap();
    store
        .apply_scan(&ScanResult {
            roots: vec![directory.path().to_path_buf()],
            min_duration_secs: 0,
            suppressed_sources: Vec::new(),
            skipped_directories: Vec::new(),
            skipped_directories_truncated: false,
            scan_incomplete: false,
            uncertainties: Vec::new(),
            records: (1..=4)
                .map(|id| ScanRecord {
                    path: directory.path().join(format!("{id}.wav")),
                    cue: None,
                    size: 1,
                    modified_ns: 1,
                    fingerprint: None,
                    media: MediaInfo {
                        title: format!("Track {id}"),
                        artist: String::new(),
                        album: String::new(),
                        duration,
                        codec: "pcm".into(),
                        channels: 2,
                        sample_rate: 48000,
                        bitrate_bps: None,
                        bitrate_exact: false,
                        track_number: None,
                        disc_number: None,
                        bits_per_sample: None,
                        release_date: None,
                    },
                })
                .collect(),
            errors: Vec::new(),
        })
        .unwrap();
    let mut state = CoreState::default();
    state.library.track_total = store.track_count().unwrap();
    state.system.config_path = directory.path().join("config.toml");
    Arc::make_mut(&mut state.system.config)
        .library_roots
        .push(directory.path().to_path_buf());
    let (scan_tx, scan_rx) = bounded(1);
    let (relocation_tx, relocation_rx) = bounded(1);
    let mut core = Core {
        store,
        engine: AudioEngine::new(1).unwrap(),
        logging: None,
        shared: Arc::new(RwLock::new(state.clone())),
        state,
        wakeup: Arc::new(RwLock::new(None)),
        subscribers: Arc::new(RwLock::new(Vec::new())),
        generation: 1,
        output_settings_request: 0,
        queue_ids: Vec::new(),
        playback: None,
        scan_tx,
        scan_rx,
        active_scan: None,
        scan_results: VecDeque::new(),
        next_scan_id: 1,
        relocation_tx,
        relocation_rx,
        active_relocation: None,
        relocation_results: VecDeque::new(),
        next_relocation_id: 1,
        played: Vec::new(),
        played_cursor: 0,
        saved_config: None,
        config_save_error: None,
    };
    core.enqueue(&[1, 2, 2, 3, 4]).unwrap();
    (directory, core)
}

#[test]
fn committed_track_edit_invalidates_before_failed_refresh_and_same_edit_is_noop() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    core.state.queue.current_id = Some(1);
    core.state.current_track = Some(Arc::new(core.track(1).unwrap()));
    core.save_queue().unwrap();
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    // SQLite accepts text in this INTEGER-affinity column; Track decoding does not.
    connection
        .execute_batch(
            "CREATE TRIGGER fail_edit_refresh AFTER UPDATE OF title_override ON tracks
         WHEN NEW.id=1 BEGIN UPDATE tracks SET channels='unreadable' WHERE id=NEW.id; END;",
        )
        .unwrap();
    let edit = || Command::EditTrack {
        track_id: 1,
        title: Some("Committed title".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    };
    let before = core.state.query_revisions();
    let error = core.command(edit()).unwrap_err();
    assert!(error.to_string().contains("Track edit committed"));
    let title: String = connection
        .query_row("SELECT title_override FROM tracks WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(title, "Committed title");
    let committed = core.state.query_revisions();
    assert_eq!(committed.library, before.library + 1);
    assert_eq!(committed.structure, before.structure);
    assert_eq!(committed.playlist, before.playlist);
    assert!(committed.queue > before.queue);
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
    assert!(matches!(
        core.view(&Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 1,
            expected: Some(before),
        }),
        Err(QueryError::RevisionChanged)
    ));
    // A retry cannot replay a committed change or disguise it as a new mutation.
    core.command(edit()).unwrap();
    assert_eq!(core.state.query_revisions(), committed);
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
    connection
        .execute_batch("DROP TRIGGER fail_edit_refresh; UPDATE tracks SET channels=2 WHERE id=1;")
        .unwrap();
    core.refresh_queue_tracks().unwrap();
    assert_eq!(core.state.current_track().unwrap().title, "Committed title");
    assert_eq!(core.state.query_revisions(), committed);
}

#[test]
fn failed_edits_and_empty_deletions_do_not_invalidate() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_edit BEFORE UPDATE OF title_override ON tracks
         BEGIN SELECT RAISE(ABORT,'edit rejected'); END;",
        )
        .unwrap();
    let before = core.state.query_revisions();
    let error = core
        .command(Command::EditTrack {
            track_id: 1,
            title: Some("Not committed".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("edit rejected"));
    assert!(!error.to_string().contains("committed"));
    assert_eq!(core.track(1).unwrap().title, "Track 1");
    assert_eq!(core.state.query_revisions(), before);
    core.command(Command::RemoveTracks {
        track_ids: Vec::new(),
    })
    .unwrap();
    core.command(Command::RemoveMissingTracks).unwrap();
    assert_eq!(core.state.query_revisions(), before);
}

#[test]
fn committed_track_deletions_invalidate_before_failed_refresh() {
    for missing in [false, true] {
        let (directory, mut core) = fixture_with_database(Some(10.0), true);
        if missing {
            let path = core.track(2).unwrap().path;
            let impacts = core
                .store
                .apply_scan(&ScanResult {
                    roots: vec![path],
                    ..ScanResult::default()
                })
                .unwrap();
            core.apply_scan_impacts(impacts).unwrap();
        }
        core.command(Command::CreatePlaylistWithTracks {
            name: "Affected".into(),
            track_ids: vec![2, 3],
        })
        .unwrap();
        core.command(Command::CreatePlaylistWithTracks {
            name: "Also affected".into(),
            track_ids: vec![2, 4],
        })
        .unwrap();
        core.command(Command::CreatePlaylistWithTracks {
            name: "Unchanged".into(),
            track_ids: vec![1, 3],
        })
        .unwrap();
        let affected = core.state.playlist_query_revisions(1);
        let also_affected = core.state.playlist_query_revisions(2);
        let unchanged = core.state.playlist_query_revisions(3);
        // The injected change targets a surviving row, not a deletion-cache entry.
        core.store.set_track_cache_page_size(0);
        core.state.queue.current_id = Some(1);
        core.state.current_track = Some(Arc::new(core.track(1).unwrap()));
        core.save_queue().unwrap();
        let current = Arc::clone(core.state.current_track.as_ref().unwrap());
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_delete_refresh AFTER DELETE ON tracks WHEN OLD.id=2
             BEGIN UPDATE tracks SET channels='unreadable' WHERE id=1; END;",
            )
            .unwrap();
        let before = core.state.query_revisions();
        let command = if missing {
            Command::RemoveMissingTracks
        } else {
            Command::RemoveTracks { track_ids: vec![2] }
        };
        let error = core.command(command).unwrap_err();
        assert!(error.to_string().contains("Track deletion committed"));
        assert!(core.store.track(2).unwrap().is_none());
        let committed = core.state.query_revisions();
        assert_eq!(committed.library, before.library + 1);
        assert_eq!(committed.structure, before.structure + 1);
        assert_eq!(committed.playlist, before.playlist + 1);
        assert!(committed.queue > before.queue);
        assert_ne!(
            core.state.playlist_query_revisions(1).playlist_entries,
            affected.playlist_entries
        );
        assert_ne!(
            core.state.playlist_query_revisions(2).playlist_entries,
            also_affected.playlist_entries
        );
        assert_eq!(
            core.state.playlist_query_revisions(3).playlist_entries,
            unchanged.playlist_entries
        );
        assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [3]);
        assert_eq!(core.store.playlist_track_ids(2, 10).unwrap(), [4]);
        assert_eq!(core.store.playlist_track_ids(3, 10).unwrap(), [1, 3]);
        assert_eq!(core.state.library.track_total, 3);
        assert_eq!(core.queue_ids, [1, 4, 5]);
        assert!(Arc::ptr_eq(
            core.state.current_track.as_ref().unwrap(),
            &current
        ));
        core.command(Command::RemoveMissingTracks).unwrap();
        assert_eq!(core.state.query_revisions(), committed);
    }
}

#[test]
fn committed_scan_invalidates_before_failed_refresh() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
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
    let error = core
        .finish_scan(ScanFinished {
            id: 1,
            result: Ok(ScanResult {
                roots: vec![path],
                ..ScanResult::default()
            }),
            import: None,
            progress: ScanProgress {
                phase: ScanPhase::Saving,
                processed: 1,
                total: Some(1),
                errors: 0,
                path: None,
            },
        })
        .unwrap_err();
    assert!(error.to_string().contains("Scan committed"));
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
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
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
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records: vec![ScanRecord {
                path: directory.path().join("committed.wav"),
                cue: None,
                size: 1,
                modified_ns: 1,
                fingerprint: Some(blake3::hash(b"committed fingerprint").to_hex().to_string()),
                media,
            }],
            ..ScanResult::default()
        })
        .unwrap();
    assert!(impacts.library && impacts.structure);
    // Commit a queued display change too, so observing its DB token proves
    // that queue refresh is still attempted after the count read fails.
    core.store
        .edit_track(1, Some("Committed queued title"), None, None, None)
        .unwrap();
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
    assert_eq!(core.state.queue, core.store.queue_state().unwrap());
}

#[test]
fn playlist_creation_count_failure_keeps_the_committed_lifetime_registered() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    let before = core.state.query_revisions();
    let playlist_id = core.store.create_playlist("Committed playlist").unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE playlists;")
        .unwrap();
    let error = core.playlist_created(playlist_id, false).unwrap_err();
    assert!(error.to_string().contains("Playlist change committed"));
    assert_ne!(core.state.library.playlist_revision, before.playlist);
    assert!(
        core.state
            .library
            .playlist_entry_revisions
            .contains_key(&playlist_id)
    );
    assert_eq!(core.state.library.playlist_entry_revisions.len(), 1);
    assert_eq!(core.state.library.playlist_total, 0);
    assert_eq!(core.state.library.revision, before.library);
    assert_eq!(
        core.state.library.duplicate_members_revision,
        before.duplicate_members
    );
    assert_eq!(core.state.queue.revision, before.queue);
}

#[test]
fn committed_play_count_invalidates_and_is_not_replayed_after_projection_failure() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    playing(&mut core, 1);
    let before_count = core.state.current_track().unwrap().play_count;
    let before = core.state.query_revisions();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_stats_refresh AFTER UPDATE OF revision ON queue_meta
         WHEN typeof(NEW.revision)='integer'
         BEGIN UPDATE queue_meta SET revision='unreadable' WHERE singleton=1; END;",
        )
        .unwrap();
    let error = core.playback_progress(8.0, 8.0).unwrap_err();
    assert!(error.to_string().contains("Playback statistics committed"));
    assert_eq!(core.state.library.revision, before.library + 1);
    assert_eq!(
        core.state.current_track().unwrap().play_count,
        before_count + 1
    );
    assert!(core.playback.as_ref().unwrap().counted);
    let count = || {
        connection
            .query_row("SELECT play_count FROM tracks WHERE id=1", [], |row| {
                row.get::<_, i64>(0)
                    .map(|value| u64::try_from(value).unwrap())
            })
            .unwrap()
    };
    assert_eq!(count(), before_count + 1);
    core.playback_progress(9.0, 9.0).unwrap();
    assert_eq!(count(), before_count + 1);
    assert_eq!(core.state.library.revision, before.library + 1);
    // Queue tokens are persisted authority, never synthesized from a failed read.
    assert_eq!(core.state.queue.revision, before.queue);
    connection
        .execute_batch("DROP TRIGGER fail_stats_refresh;")
        .unwrap();
    connection
        .execute(
            "UPDATE queue_meta SET revision=?1 WHERE singleton=1",
            [i64::try_from(before.queue + 1).unwrap()],
        )
        .unwrap();
    core.refresh_queue_tracks().unwrap();
    assert_eq!(core.state.queue.revision, before.queue + 1);
    assert_eq!(
        core.state.current_track().unwrap().play_count,
        before_count + 1
    );
}

fn relocation_fixture() -> (tempfile::TempDir, Core, i64, i64) {
    let (directory, mut core) = fixture();
    let paths = ["old.wav", "target.wav", "other-copy.wav"].map(|name| directory.path().join(name));
    let media = core
        .store
        .stored_scan_files(&[directory.path().to_path_buf()])
        .unwrap()[0]
        .media
        .clone()
        .unwrap();
    let records = paths
        .iter()
        .map(|path| {
            std::fs::write(path, b"same complete content").unwrap();
            let metadata = std::fs::metadata(path).unwrap();
            ScanRecord {
                path: path.clone(),
                cue: None,
                size: metadata.len(),
                modified_ns: i64::try_from(
                    metadata
                        .modified()
                        .unwrap()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                )
                .unwrap(),
                fingerprint: Some(blake3::hash(b"same complete content").to_hex().to_string()),
                media: media.clone(),
            }
        })
        .collect();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records,
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    let known = core
        .store
        .stored_scan_files(&[directory.path().to_path_buf()])
        .unwrap();
    let source = known
        .iter()
        .find(|file| file.path == paths[0])
        .unwrap()
        .track_id;
    let target = known
        .iter()
        .find(|file| file.path == paths[1])
        .unwrap()
        .track_id;
    std::fs::remove_file(&paths[0]).unwrap();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            roots: vec![paths[0].clone()],
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    assert!(core.track(source).unwrap().missing);
    (directory, core, source, target)
}

fn settle_relocation(core: &mut Core) -> RelocationTaskStatus {
    let event = core
        .relocation_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    core.relocation_event(event);
    assert!(core.active_relocation.is_none());
    core.state.system.relocation_task.clone().unwrap()
}

#[test]
fn manual_relocation_preserves_source_identity_state_and_references() {
    let (_directory, mut core, source, target) = relocation_fixture();
    let target_path = core.track(target).unwrap().path;
    core.command(Command::SetFavorite {
        track_ids: vec![source],
        favorite: true,
    })
    .unwrap();
    core.command(Command::EditTrack {
        track_id: source,
        title: Some("User title".into()),
        artist: None,
        album: None,
        decoder_hint: Some(DecoderHint::Native),
    })
    .unwrap();
    core.store.mark_played(source, 123).unwrap();
    core.store.increment_play_count(source).unwrap();
    core.enqueue(&[source]).unwrap();
    core.command(Command::CreatePlaylistWithTracks {
        name: "Preserved".into(),
        track_ids: vec![source],
    })
    .unwrap();
    let queue_before = core.queue_ids.clone();
    let revisions = core.state.query_revisions();
    core.command(Command::RelocateTrack {
        source_id: source,
        target_id: target,
    })
    .unwrap();
    let id = core.active_relocation.as_ref().unwrap().status.id;
    assert_ne!(core.track(source).unwrap().path, target_path);
    let status = settle_relocation(&mut core);
    assert_eq!(status.phase, RelocationPhase::Completed);
    let retained = core.track(source).unwrap();
    assert_eq!(retained.path, target_path);
    assert_eq!(retained.title, "User title");
    assert_eq!(retained.decoder_hint, DecoderHint::Native);
    assert!(retained.favorite && !retained.missing);
    assert_eq!(retained.play_count, 1);
    assert_eq!(retained.last_played, Some(123));
    assert!(core.store.track(target).unwrap().is_none());
    assert_eq!(core.queue_ids, queue_before);
    assert!(core.store.queued_track(source).unwrap().is_some());
    assert!(core.state.query_revisions().library > revisions.library);
    assert!(core.state.query_revisions().structure > revisions.structure);
    assert!(
        matches!(core.view(&Query::RelocationTask { id }), Ok(ViewResponse::RelocationTask(task)) if task == status)
    );
}

#[test]
fn relocation_cancel_and_shutdown_never_commit_completed_worker_output() {
    for shutdown in [false, true] {
        let (_directory, mut core, source, target) = relocation_fixture();
        core.relocate_track(source, target).unwrap();
        let id = core.active_relocation.as_ref().unwrap().status.id;
        if shutdown {
            core.shutdown_relocation();
        } else {
            core.cancel_relocation(id).unwrap();
            settle_relocation(&mut core);
        }
        assert_eq!(
            core.state.system.relocation_task.as_ref().unwrap().phase,
            RelocationPhase::Cancelled
        );
        assert!(core.track(source).unwrap().missing);
        assert!(core.store.track(target).unwrap().is_some());
        // A retained completed cancellation is harmless and never retries.
        core.cancel_relocation(id).unwrap();
        if let Ok(event) = core.relocation_rx.try_recv() {
            core.relocation_event(event);
        }
        assert!(core.track(source).unwrap().missing);
    }
}

#[test]
fn relocation_rechecks_target_conflicts_and_source_presence_at_commit() {
    for source_returns in [false, true] {
        let (_directory, mut core, source, target) = relocation_fixture();
        core.relocate_track(source, target).unwrap();
        // Fence hashing completion without allowing Core's commit dispatch.
        let event = core
            .relocation_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        if source_returns {
            std::fs::write(core.track(source).unwrap().path, b"returned").unwrap();
        } else {
            core.command(Command::SetFavorite {
                track_ids: vec![target],
                favorite: true,
            })
            .unwrap();
        }
        core.relocation_event(event);
        let status = core.state.system.relocation_task.as_ref().unwrap();
        assert_eq!(status.phase, RelocationPhase::Failed);
        assert!(status.error.is_some());
        assert!(core.track(source).unwrap().missing);
        assert!(core.store.track(target).unwrap().is_some());
    }
}

#[test]
fn relocation_rejects_target_file_drift_after_background_hash() {
    let (_directory, mut core, source, target) = relocation_fixture();
    core.relocate_track(source, target).unwrap();
    let event = core
        .relocation_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    std::fs::write(core.track(target).unwrap().path, b"new bytes").unwrap();
    core.relocation_event(event);
    assert_eq!(
        core.state.system.relocation_task.as_ref().unwrap().phase,
        RelocationPhase::Failed
    );
    assert!(core.track(source).unwrap().missing);
}

#[test]
fn relocation_rejects_active_affected_rows_at_start_and_during_hash() {
    for during_hash in [false, true] {
        for playing_target in [false, true] {
            let (_directory, mut core, source, target) = relocation_fixture();
            if during_hash {
                core.relocate_track(source, target).unwrap();
            }
            core.enqueue(&[if playing_target { target } else { source }])
                .unwrap();
            let queue_id = *core.queue_ids.last().unwrap();
            pending(&mut core, queue_id);
            if during_hash {
                assert_eq!(settle_relocation(&mut core).phase, RelocationPhase::Failed);
            } else {
                assert!(core.relocate_track(source, target).is_err());
                assert!(core.active_relocation.is_none());
            }
            assert!(core.track(source).unwrap().missing);
            assert!(core.store.track(target).unwrap().is_some());
        }
    }
}

#[test]
fn relocation_rejects_source_marked_independent_during_hash() {
    let (_directory, mut core, source, target) = relocation_fixture();
    core.relocate_track(source, target).unwrap();
    let event = core
        .relocation_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    core.command(Command::KeepTrackIndependent { track_id: source })
        .unwrap();
    core.relocation_event(event);
    assert_eq!(
        core.state.system.relocation_task.as_ref().unwrap().phase,
        RelocationPhase::Failed
    );
    assert!(core.track(source).unwrap().missing);
    assert!(core.store.track(target).unwrap().is_some());
}

#[test]
fn relocation_rejects_database_identity_drift_after_fresh_verification() {
    for change_source in [false, true] {
        let (directory, mut core, source, target) = relocation_fixture();
        core.relocate_track(source, target).unwrap();
        let event = core
            .relocation_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let changed_id = if change_source { source } else { target };
        let known = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()
            .into_iter()
            .find(|file| file.track_id == changed_id)
            .unwrap();
        let changed_path = known.path.clone();
        let impacts = core
            .store
            .apply_scan(&ScanResult {
                records: vec![ScanRecord {
                    path: known.path,
                    cue: None,
                    size: known.size,
                    modified_ns: known.modified_ns,
                    fingerprint: Some(
                        blake3::hash(b"different historical identity")
                            .to_hex()
                            .to_string(),
                    ),
                    media: known.media.unwrap(),
                }],
                ..ScanResult::default()
            })
            .unwrap();
        core.apply_scan_impacts(impacts).unwrap();
        if change_source {
            let impacts = core
                .store
                .apply_scan(&ScanResult {
                    roots: vec![changed_path],
                    ..ScanResult::default()
                })
                .unwrap();
            core.apply_scan_impacts(impacts).unwrap();
        }
        core.relocation_event(event);
        assert_eq!(
            core.state.system.relocation_task.as_ref().unwrap().phase,
            RelocationPhase::Failed
        );
        assert!(core.track(source).unwrap().missing);
        assert!(core.store.track(target).unwrap().is_some());
    }
}

// Capture the commands Core really sends without opening a platform device.
// The existing idle worker remains alive for its event/analysis ownership.
fn recording_audio(core: &mut Core) -> Receiver<AudioCommand> {
    let (commands, receive) = bounded(64);
    let (record, recorded) = crossbeam_channel::unbounded();
    let idle_commands = core.engine.replace_commands(commands);
    thread::spawn(move || {
        while let Ok(command) = receive.recv() {
            let snapshot = match &command {
                AudioCommand::StopAndSnapshot(reply) => Some(reply.clone()),
                _ => None,
            };
            let _ = record.send(command);
            if let Some(reply) = snapshot {
                let _ = reply.send((0, 0.0));
            }
        }
        drop(idle_commands);
    });
    recorded
}

#[test]
fn saturated_audio_admission_preserves_state_requests_events_and_command_order() {
    let (_directory, mut core) = fixture();
    let (commands, receive) = bounded(64);
    core.engine.replace_commands(commands.clone());
    core.command(Command::Device {
        name: Some("accepted-output".into()),
    })
    .unwrap();
    core.flush_config().unwrap();
    for index in 1..64 {
        commands.send(AudioCommand::Volume(index as f32)).unwrap();
    }
    let (events, event_receive) = bounded(128);
    for index in 0..128 {
        events
            .send(AudioEvent::OutputSettingsFailed {
                request: core.output_settings_request,
                device: core.state.system.selected_device.clone(),
                auto_mix: core.state.system.config.pipewire_auto_mix,
                message: format!("accepted failure {index}"),
            })
            .unwrap();
    }
    core.engine.events = event_receive;
    let saved = std::fs::read(&core.state.system.config_path).unwrap();
    let generation = core.generation;
    let (finished, completion) = bounded(1);
    let worker = thread::spawn(move || {
        let result = core.command(Command::Device {
            name: Some("rejected-output".into()),
        });
        finished
            .send((core, result))
            .unwrap_or_else(|_| panic!("Core result receiver stopped"));
    });
    let (mut core, result) = match completion.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => result,
        Err(error) => {
            // Unblock the old blocking-send implementation before failing, so
            // this deterministic regression never leaves a parked Core thread.
            drop(receive);
            worker.join().unwrap();
            panic!("saturated Core command did not return: {error}");
        }
    };
    worker.join().unwrap();
    assert!(result.unwrap_err().to_string().contains("not queued"));
    assert_eq!(core.output_settings_request, 1);
    assert_eq!(core.generation, generation);
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("accepted-output")
    );
    assert_eq!(
        std::fs::read(&core.state.system.config_path).unwrap(),
        saved
    );

    core.state.playback.position = 2.0;
    let volume = core.state.playback.volume;
    let seek_revision = core.state.playback.seek_revision;
    for (command, status) in [
        (Command::Volume { value: 0.2 }, PlaybackStatus::Playing),
        (Command::Pause, PlaybackStatus::Playing),
        (Command::Resume, PlaybackStatus::Paused),
        (Command::Seek { seconds: 5.0 }, PlaybackStatus::Playing),
    ] {
        core.state.playback.status = status;
        assert!(
            core.command(command)
                .unwrap_err()
                .to_string()
                .contains("not queued")
        );
        assert_eq!(core.state.playback.status, status);
        assert_eq!(core.state.playback.position, 2.0);
        assert_eq!(core.state.playback.seek_revision, seek_revision);
        assert_eq!(core.state.playback.volume, volume);
    }
    assert_eq!(receive.len(), 64);
    assert_eq!(core.engine.events.len(), 128);
    assert!(matches!(
        receive.try_recv().unwrap(),
        AudioCommand::OutputSettings { request: 1, device: Some(device), .. }
            if device == "accepted-output"
    ));
    for index in 1..64 {
        assert!(matches!(
            receive.try_recv().unwrap(),
            AudioCommand::Volume(value) if value == index as f32
        ));
    }
    assert!(receive.try_recv().is_err());
    for index in 0..128 {
        let event = core.engine.events.try_recv().unwrap();
        core.audio_event(event).unwrap();
        assert_eq!(
            core.state.system.last_error,
            Some(format!("accepted failure {index}"))
        );
    }
    core.command(Command::Device {
        name: Some("next-accepted-output".into()),
    })
    .unwrap();
    assert!(matches!(
        receive.try_recv().unwrap(),
        AudioCommand::OutputSettings { request: 2, .. }
    ));
    assert!(
        receive.try_recv().is_err(),
        "rejected commands must not be replayed"
    );
    drop(receive);
    assert!(core.command(Command::Volume { value: 0.3 }).is_err());
    assert_eq!(core.state.playback.volume, volume);
}

#[test]
fn busy_audio_configuration_rejects_the_whole_batch_before_persistence() {
    for available in [0, 1, 5, 6] {
        let (_directory, mut core) = fixture();
        core.state
            .system
            .config
            .save(&core.state.system.config_path)
            .unwrap();
        let baseline = core.state.clone();
        let saved = std::fs::read(&core.state.system.config_path).unwrap();
        let (commands, receive) = bounded(64);
        for index in 0..64 - available {
            commands.send(AudioCommand::Volume(index as f32)).unwrap();
        }
        core.engine.replace_commands(commands);
        let mut config = core.state.system.config.as_ref().clone();
        config.volume = 0.25;
        config.shuffle = true;
        config.still_mode = true;
        config.ffmpeg_enabled = false;
        config.output_device = Some("configured-output".into());
        let result = core.command(Command::Configure {
            config: Arc::new(config.clone()),
        });
        if available < 6 {
            assert!(result.unwrap_err().to_string().contains("not queued"));
            assert_eq!(*core.state.system.config, *baseline.system.config);
            assert_eq!(core.state.playback.volume, baseline.playback.volume);
            assert_eq!(core.state.playback.shuffle, baseline.playback.shuffle);
            assert_eq!(
                core.state.system.selected_device,
                baseline.system.selected_device
            );
            assert_eq!(core.output_settings_request, 0);
            assert_eq!(
                std::fs::read(&core.state.system.config_path).unwrap(),
                saved
            );
            assert_eq!(receive.len(), 64 - available);
            if available == 0 {
                assert!(
                    core.update_gui_config(GuiConfigChange::ToggleStillMode)
                        .is_err()
                );
                assert_eq!(*core.state.system.config, *baseline.system.config);
                assert_eq!(
                    std::fs::read(&core.state.system.config_path).unwrap(),
                    saved
                );
                assert_eq!(receive.len(), 64);
            }
        } else {
            result.unwrap();
            assert_eq!(*core.state.system.config, config);
            assert_eq!(
                Config::load(&core.state.system.config_path).unwrap(),
                config
            );
            assert_eq!(core.output_settings_request, 1);
            assert_eq!(receive.len(), 64);
        }
        for index in 0..64 - available {
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::Volume(value) if value == index as f32
            ));
        }
        if available == 6 {
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::StillMode(true)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::MediaReadBuffer(_)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::Volume(0.25)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::AnalysisSettings(_)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::FfmpegEnabled(false)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::OutputSettings { request: 1, device: Some(device), .. }
                    if device == "configured-output"
            ));
        }
        assert!(receive.try_recv().is_err());
    }
}

fn navigation_fixture(status: PlaybackStatus) -> (tempfile::TempDir, Core) {
    let (directory, mut core) = fixture();
    for id in 1..=4 {
        std::fs::write(directory.path().join(format!("{id}.wav")), []).unwrap();
    }
    let id = core.queue_ids[1];
    pending(&mut core, id);
    core.state.playback.status = status;
    if status == PlaybackStatus::Stopped {
        core.playback = None;
    }
    (directory, core)
}

fn navigation_runtime(
    mut core: Core,
) -> (
    CoreHandle,
    RuntimeControl,
    JoinHandle<Core>,
    Arc<Mutex<Vec<MprisSnapshot>>>,
) {
    core.publish();
    let (sender, requests) = bounded(64);
    let handle = CoreHandle {
        state: core.shared.clone(),
        analysis: core.engine.analysis.clone(),
        analysis_control: core.engine.analysis_control.clone(),
        waveform: core.engine.waveform.clone(),
        sender,
        wakeup: core.wakeup.clone(),
        subscribers: core.subscribers.clone(),
    };
    let publications = Arc::new(Mutex::new(Vec::new()));
    let observed = publications.clone();
    let state = core.shared.clone();
    handle.set_wakeup(move || {
        observed
            .lock()
            .push(MprisSnapshot::from_core(&state.read()));
    });
    let (control, shutdown) = RuntimeControl::new();
    let worker_control = control.clone();
    let worker = thread::spawn(move || {
        core.run(requests, shutdown, &worker_control);
        core
    });
    (handle, control, worker, publications)
}

fn assert_navigation_audio(commands: &Receiver<AudioCommand>, status: PlaybackStatus, path: &Path) {
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::StopAndSnapshot(_)
    ));
    if status != PlaybackStatus::Stopped {
        match commands.recv_timeout(Duration::from_secs(2)).unwrap() {
            AudioCommand::Load {
                path: loaded,
                paused,
                start_seconds,
                ..
            } => {
                assert_eq!(loaded, path);
                assert_eq!(paused, status == PlaybackStatus::Paused);
                assert_eq!(start_seconds, 0.0);
            }
            _ => panic!("navigation must load exactly once with the requested status"),
        }
    }
    assert!(commands.try_recv().is_err());
}

#[test]
fn navigation_endpoint_preserves_execution_status_and_occurrence_selection() {
    for status in [
        PlaybackStatus::Paused,
        PlaybackStatus::Stopped,
        PlaybackStatus::Playing,
    ] {
        for direction in [Navigation::Next, Navigation::Previous] {
            let (directory, mut core) = navigation_fixture(status);
            let expected = core.queue_ids[match direction {
                Navigation::Next => 2,
                Navigation::Previous => 0,
            }];
            let path = core
                .track(core.store.queue_track_id(expected).unwrap())
                .unwrap()
                .path;
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            let ack = handle.navigate(direction);
            assert!(ack.ok, "{:?}", ack.error);
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.playback.status, status);
            assert_eq!(snapshot.queue.current_id, Some(expected));
            assert_eq!(snapshot.playback.position, 0.0);
            assert_eq!(snapshot.current_track().unwrap().path, path);
            assert_eq!(snapshot.current_track().unwrap().play_count, 0);
            assert!(snapshot.current_track().unwrap().last_played.is_none());
            assert!(!publications.lock().is_empty());
            assert!(
                publications
                    .lock()
                    .iter()
                    .all(|state| state.playback.status == status)
            );
            assert_navigation_audio(&commands, status, &path);
            control.request_shutdown().unwrap();
            let core = worker.join().unwrap();
            assert_eq!(
                core.store
                    .queue_projection(core.queue_ids.len())
                    .unwrap()
                    .current_id,
                Some(expected)
            );
            assert!(core.store.history(MAX_QUERY_ROWS).unwrap().is_empty());
            drop(directory);
        }
    }
}

#[test]
fn pause_accepted_before_queued_eof_keeps_next_and_repeat_one_paused() {
    for repeat in [RepeatMode::Off, RepeatMode::One] {
        let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
        core.state.playback.repeat = repeat;
        let expected = core.queue_ids[if repeat == RepeatMode::One { 1 } else { 2 }];
        let generation = core.generation;
        let commands = recording_audio(&mut core);
        core.command(Command::Pause).unwrap();
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        assert_eq!(core.state.queue.current_id, Some(expected));
        // Core sends Load synchronously, but the recorder forwards it on
        // another thread. Its snapshot reply follows recording every earlier
        // FIFO command, so draining cannot race the final paused Load.
        let (snapshot, recorded_through_snapshot) = bounded(1);
        core.audio(AudioCommand::StopAndSnapshot(snapshot)).unwrap();
        recorded_through_snapshot
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let loads: Vec<_> = commands
            .try_iter()
            .filter_map(|command| match command {
                AudioCommand::Load { paused, .. } => Some(paused),
                _ => None,
            })
            .collect();
        assert_eq!(loads, [true]);
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
        assert_eq!(core.state.queue.current_id, Some(expected));
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    }
}

#[test]
fn identical_shuffle_setter_preserves_previous_occurrence_history() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    core.state.playback.shuffle = true;
    let previous = core.queue_ids[3];
    let current = core.state.queue.current_id.unwrap();
    core.played = vec![previous, current];
    core.played_cursor = 2;
    let _commands = recording_audio(&mut core);
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.navigate(Navigation::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(previous));
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert_eq!(core.played, [previous, current]);
    assert_eq!(core.played_cursor, 1);
}

#[test]
fn seek_uses_explicit_cue_end_not_estimated_whole_track_duration() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    core.seek(25.0).unwrap();
    assert_eq!(core.state.playback.position, 25.0);
    Arc::make_mut(core.state.current_track.as_mut().unwrap()).cue = Some(CueSegment {
        sheet: "album.cue".into(),
        number: 1,
        start_frame: 75,
        end_frame: Some(300),
    });
    core.seek(25.0).unwrap();
    assert_eq!(core.state.playback.position, 3.0);
}

#[test]
fn queued_pause_is_authoritative_for_navigation_not_the_callers_snapshot() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
    let expected = core.queue_ids[2];
    let commands = recording_audio(&mut core);
    let (handle, control, worker, publications) = navigation_runtime(core);
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Playing
    );
    handle.send(Command::Pause).unwrap();
    assert!(handle.navigate(Navigation::Next).ok);
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Paused
    );
    assert_eq!(handle.mpris_snapshot().queue.current_id, Some(expected));
    assert!(
        publications
            .lock()
            .iter()
            .all(|state| state.playback.status == PlaybackStatus::Paused)
    );
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Pause(true)
    ));
    let path = handle
        .mpris_snapshot()
        .current_track()
        .unwrap()
        .path
        .clone();
    assert_navigation_audio(&commands, PlaybackStatus::Paused, &path);
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn previous_resets_position_without_loading_or_changing_status() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Playing] {
        let (_directory, mut core) = navigation_fixture(status);
        let current = core.state.queue.current_id;
        core.state.playback.position = 4.0;
        let seek_revision = core.state.playback.seek_revision;
        let commands = recording_audio(&mut core);
        let (handle, control, worker, publications) = navigation_runtime(core);
        assert!(handle.navigate(Navigation::Previous).ok);
        let snapshot = handle.mpris_snapshot();
        assert_eq!(snapshot.playback.status, status);
        assert_eq!(snapshot.queue.current_id, current);
        assert_eq!(snapshot.playback.position, 0.0);
        assert_eq!(
            snapshot.playback.seek_revision,
            seek_revision.wrapping_add(1)
        );
        assert!(
            publications
                .lock()
                .iter()
                .all(|state| state.playback.status == status)
        );
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Seek(0.0)
        ));
        assert!(commands.try_recv().is_err());
        control.request_shutdown().unwrap();
        worker.join().unwrap();
    }
}

#[test]
fn shuffle_previous_preserves_status_and_follows_occurrence_history() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
        let (_directory, mut core) = navigation_fixture(status);
        let ids = core.queue_ids.clone();
        pending(&mut core, ids[3]);
        core.state.playback.status = status;
        if status == PlaybackStatus::Stopped {
            core.playback = None;
        }
        core.state.playback.shuffle = true;
        core.played = vec![ids[0], ids[2], ids[3]];
        core.played_cursor = 3;
        let commands = recording_audio(&mut core);
        let (handle, control, worker, publications) = navigation_runtime(core);
        for expected in [ids[2], ids[0]] {
            assert!(handle.navigate(Navigation::Previous).ok);
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.queue.current_id, Some(expected));
            assert_eq!(snapshot.playback.status, status);
            assert_navigation_audio(&commands, status, &snapshot.current_track().unwrap().path);
        }
        assert!(
            publications
                .lock()
                .iter()
                .all(|state| state.playback.status == status)
        );
        control.request_shutdown().unwrap();
        let core = worker.join().unwrap();
        assert_eq!(core.played, [ids[0], ids[2], ids[3]]);
        assert_eq!(core.played_cursor, 1);
    }
}

#[test]
fn existing_wire_navigation_still_starts_paused_and_stopped_tracks() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
        for command in [Command::Next, Command::Previous] {
            let (_directory, mut core) = navigation_fixture(status);
            let commands = recording_audio(&mut core);
            let (handle, control, worker, _) = navigation_runtime(core);
            assert!(handle.request_ack(command).ok);
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.playback.status, PlaybackStatus::Playing);
            assert_navigation_audio(
                &commands,
                PlaybackStatus::Playing,
                &snapshot.current_track().unwrap().path,
            );
            control.request_shutdown().unwrap();
            worker.join().unwrap();
        }
    }
}

#[test]
fn preserving_navigation_reuses_repeat_and_single_occurrence_shuffle_selection() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
        for (shuffle, repeat) in [
            (false, RepeatMode::All),
            (false, RepeatMode::One),
            (true, RepeatMode::Off),
        ] {
            let (_directory, mut core) = navigation_fixture(status);
            let expected = if shuffle {
                core.store.clear_queue().unwrap();
                core.reload_queue().unwrap();
                core.enqueue(&[1]).unwrap();
                core.queue_ids[0]
            } else if repeat == RepeatMode::All {
                core.queue_ids[0]
            } else {
                // Manual Next ignores Repeat One, just like the wire command.
                core.queue_ids[2]
            };
            let current = if shuffle {
                expected
            } else if repeat == RepeatMode::All {
                *core.queue_ids.last().unwrap()
            } else {
                core.queue_ids[1]
            };
            pending(&mut core, current);
            core.state.playback.status = status;
            if status == PlaybackStatus::Stopped {
                core.playback = None;
            }
            core.state.playback.shuffle = shuffle;
            core.state.playback.repeat = repeat;
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            assert!(handle.navigate(Navigation::Next).ok);
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.queue.current_id, Some(expected));
            assert_eq!(snapshot.playback.status, status);
            assert!(
                publications
                    .lock()
                    .iter()
                    .all(|state| state.playback.status == status)
            );
            assert_navigation_audio(&commands, status, &snapshot.current_track().unwrap().path);
            control.request_shutdown().unwrap();
            worker.join().unwrap();
        }
    }
}

#[test]
fn stopped_shuffle_previous_without_history_at_first_occurrence_is_noop() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    let current = core.queue_ids[0];
    pending(&mut core, current);
    core.playback = None;
    core.state.playback.status = PlaybackStatus::Stopped;
    core.state.playback.shuffle = true;
    let commands = recording_audio(&mut core);
    let (handle, control, worker, _) = navigation_runtime(core);
    assert!(handle.navigate(Navigation::Previous).ok);
    assert_eq!(handle.mpris_snapshot().queue.current_id, Some(current));
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Stopped
    );
    assert!(commands.try_recv().is_err());
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

fn run_until_disconnected(core: &mut Core, requests: Receiver<Request>) {
    let (control, shutdown) = RuntimeControl::new();
    core.run(requests, shutdown, &control);
}

fn display_fixture() -> (
    CoreHandle,
    crate::analysis::AnalysisWorker,
    Receiver<Request>,
) {
    let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
    let worker = crate::analysis::AnalysisWorker::new(analysis.clone()).unwrap();
    worker.configure(crate::analysis::AnalysisSettings {
        fps: 60,
        fft_size: 512,
        window: crate::config::AnalysisWindow::None,
    });
    worker.set_playback_active(true);
    let (sender, receiver) = bounded(1);
    let handle = CoreHandle {
        state: Arc::new(RwLock::new(CoreState::default())),
        analysis,
        analysis_control: worker.control.clone(),
        waveform: Arc::new(RwLock::new(audio::WaveformFrame::default())),
        sender,
        wakeup: Arc::new(RwLock::new(None)),
        subscribers: Arc::new(RwLock::new(Vec::new())),
    };
    (handle, worker, receiver)
}

#[test]
fn cancelled_queued_read_skips_store_and_does_not_cancel_another_owner() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    core.store.set_track_cache_page_size(0);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute("UPDATE tracks SET channels='unreadable' WHERE id=1", [])
        .unwrap();
    assert!(core.view(&Query::Track { track_id: 1 }).is_err());

    let mut cancelled = QueryEpoch::default();
    let mut current = QueryEpoch::default();
    let (sender, requests) = bounded(3);
    let enqueue = |track_id, ticket| {
        let (reply, response) = bounded(1);
        sender
            .send(Request {
                operation: Operation::Query {
                    query: Query::Track { track_id },
                    ticket,
                },
                reply: Some(reply),
            })
            .unwrap();
        response
    };
    let stale = enqueue(1, Some(cancelled.ticket()));
    let independent = enqueue(2, Some(current.ticket()));
    let unconditional = enqueue(1, None);
    cancelled.cancel();
    drop(sender);
    run_until_disconnected(&mut core, requests);

    assert!(matches!(
        stale.recv().unwrap(),
        CoreResponse::Query(QueryResponse {
            result: Err(QueryError::Cancelled),
            ..
        })
    ));
    assert!(matches!(
        independent.recv().unwrap(),
        CoreResponse::Query(QueryResponse {
            result: Ok(ViewResponse::Track(Some(track))),
            ..
        }) if track.id == 2
    ));
    assert!(matches!(
        unconditional.recv().unwrap(),
        CoreResponse::Query(QueryResponse {
            result: Err(QueryError::Message(_)),
            ..
        })
    ));
}

#[test]
fn already_cancelled_read_never_enters_the_request_queue() {
    let (handle, _analysis, requests) = display_fixture();
    let mut epoch = QueryEpoch::default();
    let ticket = epoch.ticket();
    epoch.cancel();
    assert!(matches!(
        handle
            .query_if_current(Query::Track { track_id: 1 }, ticket)
            .result,
        Err(QueryError::Cancelled)
    ));
    assert!(requests.try_recv().is_err());
}

#[test]
fn cancellation_after_dispatch_waits_for_the_physical_reply() {
    use std::sync::atomic::AtomicBool;

    let (_directory, core) = fixture();
    let (handle, _analysis, requests) = display_fixture();
    let mut epoch = QueryEpoch::default();
    let ticket = epoch.ticket();
    let gate = Arc::new(AtomicBool::new(true));
    let worker_gate = Arc::clone(&gate);
    let (finished, completion) = bounded(1);
    let worker = thread::spawn(move || {
        let response = handle.query_if_current(Query::Track { track_id: 1 }, ticket);
        worker_gate.store(false, Ordering::Release);
        finished.send(response).unwrap();
    });
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let Operation::Query { query, ticket } = request.operation else {
        panic!("expected a read request");
    };
    // Execute the same dispatch boundary as Core::run, then deliberately
    // hold physical completion delivery until after cancellation.
    let response = core.query_response_if_current(&query, ticket.as_ref());
    assert!(matches!(&response.result, Ok(ViewResponse::Track(Some(track))) if track.id == 1));
    epoch.cancel();
    assert!(gate.load(Ordering::Acquire));
    assert!(completion.try_recv().is_err());
    request
        .reply
        .unwrap()
        .send(CoreResponse::Query(response))
        .unwrap();
    assert!(matches!(
        completion.recv_timeout(Duration::from_secs(2)).unwrap().result,
        Ok(ViewResponse::Track(Some(track))) if track.id == 1
    ));
    worker.join().unwrap();
    assert!(!gate.load(Ordering::Acquire));
}

fn display_wait(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < deadline, "display transition timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

fn display_samples(
    producer: &mut ringbuf::HeapProd<crate::analysis::TapFrame>,
    worker: &crate::analysis::AnalysisWorker,
    demand_token: u64,
    count: usize,
    samples: [f32; 2],
) {
    use ringbuf::traits::Producer;
    for _ in 0..count {
        assert!(
            producer
                .try_push(crate::analysis::TapFrame {
                    samples,
                    epoch: worker.control.epoch.load(Ordering::Acquire),
                    demand_token,
                    gap: false,
                })
                .is_ok()
        );
    }
}

#[test]
fn display_off_clears_results_with_a_saturated_business_queue_and_reopens() {
    let (handle, worker, _receiver) = display_fixture();
    let demand = DisplayDemand {
        enabled: true,
        visualizer: crate::analysis::VisualizerDemand {
            stereo_density: true,
            ..Default::default()
        },
    };
    handle.set_display_demand(demand);
    let mut producer = worker.producer();
    display_samples(
        &mut producer,
        &worker,
        worker.control.display_token(),
        512,
        [0.5; 2],
    );
    handle.request_analysis_frame();
    display_wait(|| handle.analysis.read().settings.is_some());
    handle.send(Command::DismissError).unwrap();
    // This is the pre-fix close path: its off request is rejected at capacity.
    assert!(handle.send(Command::Analysis { enabled: false }).is_err());
    handle.set_display_demand(DisplayDemand::default());
    display_wait(|| handle.analysis.read().settings.is_none());
    {
        let frame = handle.analysis.read();
        assert!(frame.spectrum_db.is_empty());
        assert!(frame.visualizer.density.is_empty());
        assert_eq!(frame.visualizer.demand, Default::default());
    }
    handle.set_display_demand(demand);
    display_samples(
        &mut producer,
        &worker,
        worker.control.display_token(),
        512,
        [0.0; 2],
    );
    handle.request_analysis_frame();
    display_wait(|| handle.analysis.read().settings.is_some());
    let frame = handle.analysis.read();
    assert!(handle.analysis_frame_is_current(&frame));
    assert_eq!(frame.visualizer.demand, demand.visualizer);
    assert_eq!([frame.rms_left, frame.rms_right], [0.0; 2]);
}

#[test]
fn display_quick_off_on_rejects_in_flight_and_queued_old_spectrum() {
    use ringbuf::traits::Observer;
    let (handle, worker, _receiver) = display_fixture();
    let demand = DisplayDemand {
        enabled: true,
        ..Default::default()
    };
    handle.set_display_demand(demand);
    let old_token = worker.control.display_token();
    let mut producer = worker.producer();
    display_samples(&mut producer, &worker, old_token, 512, [0.25; 2]);
    handle.request_analysis_frame();
    display_wait(|| handle.analysis.read().settings.is_some());
    let held_frame = handle.analysis.write();
    let sequence = held_frame.sequence;
    display_samples(&mut producer, &worker, old_token, 512, [1.0; 2]);
    handle.request_analysis_frame();
    display_wait(|| producer.occupied_len() == 0);
    handle.set_display_demand(DisplayDemand::default());
    handle.set_display_demand(demand);
    assert!(!handle.analysis_frame_is_current(&held_frame));
    drop(held_frame);
    display_wait(|| handle.analysis.read().sequence > sequence);
    assert_eq!(handle.analysis.read().settings, None);
    display_samples(&mut producer, &worker, old_token, 512, [1.0; 2]);
    display_samples(
        &mut producer,
        &worker,
        worker.control.display_token(),
        511,
        [0.0; 2],
    );
    handle.request_analysis_frame();
    display_wait(|| producer.occupied_len() == 0);
    assert_eq!(handle.analysis.read().settings, None);
    display_samples(
        &mut producer,
        &worker,
        worker.control.display_token(),
        1,
        [0.0; 2],
    );
    handle.request_analysis_frame();
    display_wait(|| handle.analysis.read().settings.is_some());
    let frame = handle.analysis.read();
    assert!(handle.analysis_frame_is_current(&frame));
    assert_eq!([frame.rms_left, frame.rms_right], [0.0; 2]);
}

#[test]
fn metadata_intents_merge_and_pages_invalidate_only_for_queued_changes() {
    for title_first in [true, false] {
        let (_directory, mut core) = fixture();
        core.store.set_queue_current(Some(1)).unwrap();
        core.refresh_queue_tracks().unwrap();
        let title = Command::EditTrack {
            track_id: 1,
            title: Some("Title".into()),
            artist: None,
            album: None,
            decoder_hint: Some(crate::model::DecoderHint::Ffmpeg),
        };
        let artist = Command::EditTrack {
            track_id: 1,
            title: None,
            artist: Some("Artist".into()),
            album: None,
            decoder_hint: None,
        };
        let revision = core.state.queue.revision;
        for command in if title_first {
            [title, artist]
        } else {
            [artist, title]
        } {
            core.command(command).unwrap();
        }
        assert_ne!(core.state.queue.revision, revision);
        let current = core.state.current_track().unwrap();
        assert_eq!((&*current.title, &*current.artist), ("Title", "Artist"));
        assert_eq!(current.decoder_hint, crate::model::DecoderHint::Ffmpeg);
        let page = core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap();
        assert_eq!(
            (&*page.rows[0].track.title, &*page.rows[0].track.artist),
            ("Title", "Artist")
        );
        core.remove_queue_entries(&[5]).unwrap();
        core.publish();
        let before = core.state.revisions;
        let queue = core.state.queue.clone();
        core.command(Command::EditTrack {
            track_id: 4,
            title: Some("Unqueued".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        core.publish();
        assert_eq!(core.state.queue, queue);
        assert_eq!(
            core.state.revisions.changed_since(before),
            StateSections {
                library: true,
                ..StateSections::default()
            }
        );
        core.command(Command::SetFavorite {
            track_ids: vec![2, 2, 3],
            favorite: true,
        })
        .unwrap();
        let rows = core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap().rows;
        assert!(rows[1].track.favorite && rows[2].track.favorite && rows[3].track.favorite);
        assert!(!rows[0].track.favorite);
    }
}

#[test]
fn unchanged_metadata_favorite_and_playlist_intents_preserve_revisions() {
    let (_directory, mut core) = fixture();
    core.store.set_queue_current(Some(1)).unwrap();
    core.refresh_queue_tracks().unwrap();
    core.command(Command::CreatePlaylist { name: "Mix".into() })
        .unwrap();
    // Creating an equal-value override changes member facts, not display payload.
    let before = core.state.query_revisions();
    let before_current = Arc::clone(core.state.current_track.as_ref().unwrap());
    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("Track 1".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    let created = core.state.query_revisions();
    assert_ne!(created.duplicate_members, before.duplicate_members);
    assert_eq!(
        QueryRevisions {
            duplicate_members: before.duplicate_members,
            ..created
        },
        before
    );
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &before_current
    ));
    core.publish();
    let revisions = core.state.query_revisions();
    let sections = core.state.revisions;
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    for command in [
        Command::EditTrack {
            track_id: 1,
            title: None,
            artist: None,
            album: None,
            decoder_hint: None,
        },
        Command::EditTrack {
            track_id: 1,
            title: Some("Track 1".into()),
            artist: None,
            album: None,
            decoder_hint: Some(DecoderHint::Auto),
        },
        Command::SetFavorite {
            track_ids: vec![1, 1, 2],
            favorite: false,
        },
        Command::SetFavorite {
            track_ids: vec![],
            favorite: true,
        },
        Command::RenamePlaylist {
            playlist_id: 1,
            name: "Mix".into(),
        },
    ] {
        core.command(command).unwrap();
        core.publish();
        assert_eq!(core.state.query_revisions(), revisions);
        assert_eq!(core.state.revisions, sections);
        assert!(Arc::ptr_eq(
            core.state.current_track.as_ref().unwrap(),
            &current
        ));
    }
    for command in [
        Command::EditTrack {
            track_id: 999,
            title: None,
            artist: None,
            album: None,
            decoder_hint: None,
        },
        Command::SetFavorite {
            track_ids: vec![1, 999],
            favorite: true,
        },
        Command::RenamePlaylist {
            playlist_id: 999,
            name: "Mix".into(),
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.query_revisions(), revisions);
        assert!(!core.track(1).unwrap().favorite);
    }
}

#[test]
fn decoder_hint_intent_refreshes_current_without_invalidating_queue_or_restarting_audio() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 3);
    core.publish();
    let revisions = core.state.query_revisions();
    let sections = core.state.revisions;
    let playback = core.state.playback.clone();
    let generation = core.generation;
    for hint in [DecoderHint::Ffmpeg, DecoderHint::Native, DecoderHint::Auto] {
        core.command(Command::EditTrack {
            track_id: 2,
            title: None,
            artist: None,
            album: None,
            decoder_hint: Some(hint),
        })
        .unwrap();
        core.publish();
        assert_eq!(core.state.current_track().unwrap().decoder_hint, hint);
        assert_eq!(core.track(2).unwrap().decoder_hint, hint);
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.state.queue.revision, revisions.queue);
        assert_eq!(core.state.playback, playback);
        assert_eq!(core.generation, generation);
        assert!(core.playback.is_some());
        let query = Query::QueueBatch {
            query: None,
            after: None,
            limit: 5,
            expected: Some(revisions),
        };
        assert!(matches!(core.view(&query), Ok(ViewResponse::QueueBatch(_))));
        let before_repeat = core.state.revisions;
        core.command(Command::EditTrack {
            track_id: 2,
            title: None,
            artist: None,
            album: None,
            decoder_hint: Some(hint),
        })
        .unwrap();
        core.publish();
        assert_eq!(core.state.revisions, before_repeat);
    }
    assert_ne!(core.state.library.revision, revisions.library);
    assert_ne!(core.state.revisions.playback, sections.playback);
    assert_eq!(core.state.revisions.queue, sections.queue);
}

#[test]
fn activity_updates_playback_but_only_display_statistics_invalidate_queue_pages() {
    let (_directory, mut core) = fixture();
    core.store.set_queue_current(Some(1)).unwrap();
    core.refresh_queue_tracks().unwrap();
    let revision = core.state.queue.revision;
    let stats = core.store.mark_played(1, 7).unwrap();
    core.update_track_stats(1, stats).unwrap();
    assert_eq!(core.state.current_track().unwrap().last_played, Some(7));
    assert_eq!(core.state.queue.revision, revision);
    let stats = core.store.increment_play_count(1).unwrap();
    core.update_track_stats(1, stats).unwrap();
    assert_ne!(core.state.queue.revision, revision);
    assert_eq!(
        core.store.queue_page(None, 0, 1).unwrap().rows[0]
            .track
            .play_count,
        1
    );
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
        records: (1..=MAX_QUERY_ROWS + 1)
            .map(|id| ScanRecord {
                path: directory.path().join(format!("history-{id}.wav")),
                cue: None,
                size: 1,
                modified_ns: 1,
                fingerprint: None,
                media: media.clone(),
            })
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
fn sql_queue_restores_order_duplicates_current_and_clear_does_not_reuse_ids() {
    let (_directory, mut core) = fixture();
    core.state.queue.current_id = Some(3);
    core.save_queue().unwrap();
    core.queue_ids.clear();
    core.state.queue = QueueState::default();
    core.load_initial_state().unwrap();
    assert_eq!(
        queue_entries(&core),
        [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)]
    );
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(2));
    assert_eq!(core.state.current_track().unwrap().id, 2);
    core.command(Command::ClearQueue).unwrap();
    core.enqueue(&[1]).unwrap();
    assert!(core.queue_ids[0] > 5);
    core.load_initial_state().unwrap();
    assert_eq!(core.state.queue.total, 1);
    assert!(core.state.current_track.is_none());
}

#[test]
fn playlist_mutations_preserve_track_traversal_and_playback_sections() {
    let (_directory, mut core) = fixture();
    core.publish();
    let track_revision = core.state.query_revisions();
    let sections = core.state.revisions;
    let operations = [
        (
            Command::CreatePlaylist {
                name: "empty".into(),
            },
            1,
            false,
        ),
        (
            Command::CreatePlaylistWithTracks {
                name: "mix".into(),
                track_ids: vec![1, 2],
            },
            2,
            true,
        ),
        (
            Command::RenamePlaylist {
                playlist_id: 2,
                name: "renamed".into(),
            },
            2,
            false,
        ),
        (
            Command::AddPlaylist {
                playlist_id: 2,
                track_ids: vec![3],
            },
            2,
            true,
        ),
        (
            Command::AddPlaylistSources {
                playlist_id: 2,
                directories: vec![],
                track_ids: vec![4],
            },
            2,
            true,
        ),
        (Command::RemovePlaylistEntry { entry_id: 1 }, 2, true),
        (Command::DeletePlaylist { playlist_id: 1 }, 1, false),
        (Command::DeletePlaylist { playlist_id: 2 }, 0, true),
    ];
    for (command, total, members_changed) in operations {
        let before = core.state.query_revisions();
        core.command(command).unwrap();
        core.publish();
        assert_eq!(core.state.library.playlist_total, total);
        let after = core.state.query_revisions();
        assert_eq!(after.library, track_revision.library);
        assert_eq!(after.duplicate_groups, before.duplicate_groups);
        assert_eq!(
            after.duplicate_members != before.duplicate_members,
            members_changed
        );
        assert_eq!(
            core.state.query_revisions().structure,
            track_revision.structure
        );
        assert_eq!(core.state.revisions.queue, sections.queue);
        assert_eq!(core.state.revisions.playback, sections.playback);
        let ViewResponse::TrackBatch(batch) = core
            .view(&Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: Some(1),
                limit: 2,
                expected: Some(track_revision),
            })
            .unwrap()
        else {
            panic!("expected track batch")
        };
        assert_eq!(
            batch.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(
            core.view(&Query::PlaylistSummaryBatch {
                query: None,
                after: None,
                limit: 2,
                expected: Some(before),
            })
            .is_err()
        );
    }
}

#[test]
fn unchanged_scans_keep_query_results_valid_and_still_import_playlists() {
    let (directory, mut core) = fixture();
    core.store.set_queue_current(Some(1)).unwrap();
    core.refresh_queue_tracks().unwrap();
    core.command(Command::CreatePlaylistWithTracks {
        name: "retained".into(),
        track_ids: vec![2, 3],
    })
    .unwrap();
    let retained = core.state.playlist_query_revisions(1);
    let result = ScanResult {
        roots: vec![directory.path().to_path_buf()],
        records: core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()
            .into_iter()
            .map(|known| ScanRecord {
                path: known.path,
                cue: known.cue,
                size: known.size,
                modified_ns: known.modified_ns,
                fingerprint: known.fingerprint,
                media: known.media.unwrap(),
            })
            .collect(),
        ..ScanResult::default()
    };
    let before = core.state.query_revisions();
    let current = core.state.current_track.clone().unwrap();
    for import in [false, true] {
        let scan = ScanFinished {
            id: 0,
            result: Ok(result.clone()),
            import: import.then(|| {
                (
                    "unchanged import".into(),
                    vec![M3uItem {
                        path: directory.path().join("1.wav"),
                        name: None,
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
        assert!(core.finish_scan(scan).unwrap());
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
            .track_id_for_source(&source, None)
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
        let (directory, mut core) = fixture_with_database(Some(10.0), true);
        let source = directory.path().join("new.wav");
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
                .track_id_for_source(&source, None)
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
            0
        );
    }
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
        id: 0,
        result: Ok(ScanResult {
            records: vec![ScanRecord {
                path: directory.path().join("1.wav"),
                size: 1,
                modified_ns: 2,
                fingerprint: None,
                cue: None,
                media,
            }],
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
    let error = core.finish_scan(scan).unwrap_err();
    assert!(error.to_string().contains("Scan saved"));
    assert!(error.to_string().contains("playlist creation"));
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
            cue: Some(crate::model::CueSegment {
                sheet: sheet.clone(),
                number: 1,
                start_frame: 0,
                end_frame: Some(75),
            }),
            media: media.clone(),
        };
        let impacts = core
            .store
            .apply_scan(&ScanResult {
                records: vec![old],
                ..ScanResult::default()
            })
            .unwrap();
        core.apply_scan_impacts(impacts).unwrap();
        let old_id = core
            .store
            .track_id_for_source(&sheet, Some(1))
            .unwrap()
            .unwrap();
        let path = directory.path().join("2.wav");
        let scan = ScanFinished {
            id: 0,
            result: Ok(ScanResult {
                records: if readable {
                    vec![ScanRecord {
                        path: path.clone(),
                        size: 1,
                        modified_ns: 1,
                        fingerprint: None,
                        cue: None,
                        media,
                    }]
                } else {
                    vec![]
                },
                errors: vec!["CUE sheet could not be read".into()],
                ..ScanResult::default()
            }),
            import: Some((
                "partial".into(),
                vec![
                    M3uItem {
                        path: sheet,
                        name: None,
                        cue_track: Some(1),
                    },
                    M3uItem {
                        path,
                        name: None,
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
        let result = core.finish_scan(scan);
        assert!(core.store.track(old_id).unwrap().is_some());
        if readable {
            assert!(!result.unwrap());
            let rows = core
                .store
                .playlist_entries_page(1, None, 0, MAX_QUERY_ROWS)
                .unwrap()
                .rows;
            assert_eq!(rows.iter().map(|row| row.track_id).collect::<Vec<_>>(), [2]);
        } else {
            assert!(result.is_err());
            assert_eq!(core.state.library.playlist_total, 0);
        }
    }
}

#[test]
fn config_failure_retains_values_and_recovers_without_clearing_other_errors() {
    for unrelated in [false, true] {
        let (directory, mut core) = fixture();
        let path = core.state.system.config_path.clone();
        core.flush_config().unwrap();
        let backup = directory.path().join("config.good");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        core.command(Command::Volume { value: 0.25 }).unwrap();
        assert!(core.flush_config().is_err());
        core.publish();
        assert!(core.shared.read().system.last_error.is_some());
        assert_eq!(core.state.playback.volume, 0.25);
        assert_ne!(core.state.system.config.volume, 0.25);
        assert_ne!(Config::load(&backup).unwrap().volume, 0.25);
        if unrelated {
            core.state.system.last_error = Some("Unrelated failure".into());
        } else {
            core.command(Command::DismissError).unwrap();
        }
        // Retrying the same failure must not replace another error or a dismissal.
        let expected = core.state.system.last_error.clone();
        assert!(core.flush_config().is_err());
        assert_eq!(core.state.system.last_error, expected);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        assert!(core.flush_config().unwrap());
        assert_eq!(Config::load(&path).unwrap().volume, 0.25);
        assert_eq!(core.state.system.last_error, expected);
        assert!(!core.flush_config().unwrap());
    }
}

#[test]
fn successful_config_retry_clears_only_its_own_failure() {
    let (_directory, mut core) = fixture();
    let path = core.state.system.config_path.clone();
    std::fs::create_dir(&path).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    assert!(core.flush_config().is_err());
    assert!(core.state.system.last_error.is_some());
    std::fs::remove_dir(&path).unwrap();
    assert!(core.flush_config().unwrap());
    assert!(core.state.system.last_error.is_none());
    assert!(Config::load(&path).unwrap().shuffle);
}

#[test]
fn shutdown_flush_reports_failure_and_retains_live_settings() {
    let (_directory, mut core) = fixture();
    let path = core.state.system.config_path.clone();
    std::fs::create_dir(&path).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    let (sender, receiver) = bounded(1);
    drop(sender);
    run_until_disconnected(&mut core, receiver);
    let state = core.shared.read();
    assert!(state.system.shutting_down);
    assert!(state.system.last_error.is_some());
    assert_eq!(state.playback.repeat, RepeatMode::All);
    assert_eq!(state.system.config.repeat, RepeatMode::Off);
}

#[test]
fn still_mode_toggle_preserves_unsaved_playback_and_device_settings() {
    let (_directory, mut core) = fixture();
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    // A live output selection must survive unrelated quick changes.
    core.state.system.selected_device = Some("pending-output".into());
    for enabled in [true, false] {
        core.update_gui_config(GuiConfigChange::ToggleStillMode)
            .unwrap();
        assert_eq!(core.state.system.config.still_mode, enabled);
        assert_eq!(core.state.playback.volume, 0.25);
        assert!(core.state.playback.shuffle);
        assert_eq!(core.state.playback.repeat, RepeatMode::All);
        assert_eq!(
            core.state.system.selected_device.as_deref(),
            Some("pending-output")
        );
        assert!(!core.state.system.config_path.exists());
    }
}

#[test]
fn library_column_toggles_merge_latest_config_without_audio_side_effects() {
    let (_directory, mut core) = fixture();
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    core.state.system.selected_device = Some("pending-output".into());
    let config = Arc::make_mut(&mut core.state.system.config);
    config.ui_scale = 1.25;
    config.waveform_seek_enabled = false;
    config.library_columns = crate::config::LibraryColumns {
        artist: false,
        album: false,
        duration: true,
        codec: false,
        bitrate: false,
    };
    let audio = recording_audio(&mut core);
    let mut expected = core.state.system.config.as_ref().clone();
    expected.volume = 0.25;
    expected.shuffle = true;
    expected.repeat = RepeatMode::All;
    expected.output_device = Some("pending-output".into());
    for (column, columns) in [
        (
            LibraryColumn::Duration,
            crate::config::LibraryColumns {
                artist: false,
                album: false,
                duration: false,
                codec: false,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Artist,
            crate::config::LibraryColumns {
                artist: true,
                album: false,
                duration: false,
                codec: false,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Album,
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: false,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Codec,
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Bitrate,
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
        (
            LibraryColumn::Artist,
            crate::config::LibraryColumns {
                artist: false,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
    ] {
        core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(column))
            .unwrap();
        expected.library_columns = columns;
        assert_eq!(*core.state.system.config, expected);
        assert!(!core.state.system.config_path.exists());
        assert_eq!(core.state.playback.volume, expected.volume);
        assert_eq!(core.state.playback.shuffle, expected.shuffle);
        assert_eq!(core.state.playback.repeat, expected.repeat);
        assert_eq!(core.state.system.selected_device, expected.output_device);
        core.publish();
        assert_eq!(*core.shared.read().system.config, expected);
        assert!(matches!(
            audio.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
    }
}

#[test]
fn library_column_toggle_commits_without_accessing_unwritable_config_path() {
    let (_directory, mut core) = fixture();
    let audio = recording_audio(&mut core);
    Arc::make_mut(&mut core.state.system.config)
        .library_columns
        .duration = true;
    std::fs::create_dir(&core.state.system.config_path).unwrap();
    core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(
        LibraryColumn::Duration,
    ))
    .unwrap();
    core.publish();
    assert!(!core.shared.read().system.config.library_columns.duration);
    assert!(core.state.system.last_error.is_none());
    assert!(audio.try_recv().is_err());
    assert!(core.state.system.config_path.is_dir());
}

#[test]
fn overlapping_root_changes_leave_published_and_saved_configuration_unchanged() {
    let (directory, mut core) = fixture();
    core.state
        .system
        .config
        .save(&core.state.system.config_path)
        .unwrap();
    let before = core.state.system.config.clone();
    let saved = std::fs::read(&core.state.system.config_path).unwrap();
    for root in [directory.path().join("child"), directory.path().join("..")] {
        assert!(
            core.update_gui_config(GuiConfigChange::AddLibraryRoot(root.clone()))
                .is_err()
        );
        let mut config = before.as_ref().clone();
        config.library_roots.push(root);
        assert!(
            core.command(Command::Configure {
                config: Arc::new(config)
            })
            .is_err()
        );
        assert_eq!(*core.state.system.config, *before);
        assert_eq!(
            std::fs::read(&core.state.system.config_path).unwrap(),
            saved
        );
    }
}

#[test]
fn desktop_config_edits_preserve_live_playback_and_merge_roots() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.system.config).library_roots[0] =
        directory.path().join("existing");
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    core.state.system.selected_device = Some("pending-output".into());
    let root = directory.path().join("added");
    let seek = core.state.system.config.waveform_seek_enabled;
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(root.clone()))
        .unwrap();
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(root))
        .unwrap();
    assert_eq!(core.state.system.config.library_roots.len(), 2);
    assert_eq!(core.state.system.config.waveform_seek_enabled, !seek);
    assert_eq!(core.state.playback.volume, 0.25);
    assert!(core.state.playback.shuffle);
    assert_eq!(core.state.playback.repeat, RepeatMode::All);
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("pending-output")
    );
    assert!(!core.state.system.config_path.exists());
    assert!(core.flush_config().unwrap());
    let saved = Config::load(&core.state.system.config_path).unwrap();
    assert_eq!(saved.volume, 0.25);
    assert!(saved.shuffle);
    assert_eq!(saved.repeat, RepeatMode::All);
    assert_eq!(saved.output_device.as_deref(), Some("pending-output"));
    assert_eq!(saved.library_roots.len(), 2);
}

#[test]
fn settings_apply_merges_every_unedited_field_at_execution() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.system.config).library_roots[0] =
        directory.path().join("existing");
    let _audio = recording_audio(&mut core);
    let baseline = core.state.system.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.scan_max_depth = 7;
    let patch = ConfigPatch::between(&baseline, &draft);
    core.command(Command::Volume { value: 0.2 }).unwrap();
    core.command(Command::Device {
        name: Some("live-output".into()),
    })
    .unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::One,
    })
    .unwrap();
    let added = directory.path().join("added");
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(added.clone()))
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleStillMode)
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    for column in LibraryColumn::ALL {
        core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(column))
            .unwrap();
    }
    let mut expected = core.state.system.config.as_ref().clone();
    expected.scan_max_depth = 7;
    // These playback changes are still newer than the in-memory config snapshot.
    core.command(Command::Volume { value: 0.3 }).unwrap();
    core.command(Command::Device {
        name: Some("pending-output".into()),
    })
    .unwrap();
    expected.volume = 0.3;
    expected.output_device = Some("pending-output".into());
    assert!(!core.state.system.config_path.exists());
    core.update_gui_config(GuiConfigChange::Settings(Box::new(patch)))
        .unwrap();
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(*visible.system.config, expected);
    assert_eq!(visible.playback.volume, 0.3);
    assert!(visible.playback.shuffle);
    assert_eq!(visible.playback.repeat, RepeatMode::One);
    assert!(visible.system.config.library_roots.contains(&added));
    assert_eq!(
        Config::load(&core.state.system.config_path).unwrap(),
        expected
    );
}

#[test]
fn settings_apply_explicit_fields_override_newer_conflicting_values() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.system.config).library_roots[0] =
        directory.path().join("existing");
    let _audio = recording_audio(&mut core);
    core.command(Command::Device {
        name: Some("original-output".into()),
    })
    .unwrap();
    core.flush_config().unwrap();
    let baseline = core.state.system.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.volume = 0.4;
    draft.output_device = None;
    draft.library_roots.clear();
    draft.still_mode = true;
    let patch = ConfigPatch::between(&baseline, &draft);
    core.command(Command::Volume { value: 0.2 }).unwrap();
    core.command(Command::Device {
        name: Some("external-output".into()),
    })
    .unwrap();
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(
        directory.path().join("external"),
    ))
    .unwrap();
    core.update_gui_config(GuiConfigChange::Settings(Box::new(patch)))
        .unwrap();
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(visible.playback.volume, 0.4);
    assert_eq!(visible.system.selected_device, None);
    assert!(visible.system.config.library_roots.is_empty());
    assert!(visible.system.config.still_mode);
    assert_eq!(
        Config::load(&core.state.system.config_path).unwrap(),
        *visible.system.config
    );
}

#[test]
fn settings_failed_save_retry_merges_against_latest_runtime() {
    let (_directory, mut core) = fixture();
    let audio = recording_audio(&mut core);
    let baseline = core.state.system.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.scan_max_depth = 7;
    let patch = ConfigPatch::between(&baseline, &draft);
    std::fs::create_dir(&core.state.system.config_path).unwrap();
    assert!(
        core.update_gui_config(GuiConfigChange::Settings(Box::new(patch.clone())))
            .is_err()
    );
    assert_eq!(*core.state.system.config, *baseline);
    assert!(audio.try_recv().is_err());
    std::fs::remove_dir(&core.state.system.config_path).unwrap();
    core.command(Command::Volume { value: 0.2 }).unwrap();
    core.update_gui_config(GuiConfigChange::Settings(Box::new(patch)))
        .unwrap();
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(visible.playback.volume, 0.2);
    assert_eq!(visible.system.config.scan_max_depth, 7);
    assert_eq!(
        Config::load(&core.state.system.config_path).unwrap(),
        *visible.system.config
    );
}

#[test]
fn settings_saved_audio_failure_keeps_committed_desired_configuration() {
    let (_directory, mut core) = fixture();
    let (commands, receiver) = bounded(64);
    drop(receiver);
    core.engine.replace_commands(commands);
    let baseline = core.state.system.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.scan_max_depth = 7;
    let error = core
        .update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
            &baseline, &draft,
        ))))
        .unwrap_err();
    assert!(format!("{error:#}").contains("Configuration saved, but audio application failed"));
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(visible.system.config.scan_max_depth, 7);
    assert_eq!(
        Config::load(&core.state.system.config_path).unwrap(),
        *visible.system.config
    );
}

#[test]
fn playback_publication_does_not_invalidate_unchanged_sections() {
    let (_directory, mut core) = fixture();
    core.publish();
    let before = core.state.revisions;
    let wakeup_revision = core.shared.read().system.revision;
    core.state.playback.position = 2.0;
    core.publish();
    assert_eq!(
        core.state.revisions.changed_since(before),
        StateSections {
            playback: true,
            ..StateSections::default()
        }
    );
    {
        let published = core.shared.read();
        assert_eq!(published.revisions.system, before.system);
        assert_ne!(published.system.revision, wakeup_revision);
    }
    let before = core.state.revisions;
    core.state.system.last_error = Some("failed scan".into());
    core.publish();
    assert_eq!(
        core.state.revisions.changed_since(before),
        StateSections {
            system: true,
            ..StateSections::default()
        }
    );
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

#[test]
fn playlist_group_commands_reject_stale_intents_and_preserve_noop_revisions() {
    let (_directory, mut core) = fixture();
    for name in ["one", "two"] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids: vec![1, 2, 3, 4],
        })
        .unwrap();
    }
    let entries = core
        .store
        .playlist_entries_page(1, None, 0, 4)
        .unwrap()
        .rows;
    let ids: Vec<_> = entries.iter().map(|row| row.id).collect();
    let baseline = core.state.playlist_query_revisions(1);
    let token = baseline.playlist_entries.unwrap().revision;
    for command in [
        Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![],
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0], ids[1]],
            before_entry_id: Some(ids[2]),
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0], ids[2]],
            before_entry_id: Some(ids[2]),
            expected_revision: token,
        },
    ] {
        core.command(command).unwrap();
        assert_eq!(core.state.playlist_query_revisions(1), baseline);
    }
    for command in [
        Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0], i64::MAX],
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0]],
            before_entry_id: Some(i64::MAX),
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 999,
            entry_ids: vec![],
            before_entry_id: None,
            expected_revision: token,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.playlist_query_revisions(1), baseline);
        assert_eq!(
            core.store
                .playlist_entries_page(1, None, 0, 4)
                .unwrap()
                .rows,
            entries
        );
    }
    let other = core
        .store
        .playlist_entries_page(2, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::MovePlaylistEntry {
        entry_id: other,
        index: 3,
    })
    .unwrap();
    assert_eq!(core.state.playlist_query_revisions(1), baseline);
    core.command(Command::MovePlaylistEntries {
        playlist_id: 1,
        entry_ids: vec![ids[2], ids[0]],
        before_entry_id: None,
        expected_revision: token,
    })
    .unwrap();
    let reordered = core.state.playlist_query_revisions(1);
    assert_eq!(reordered.playlist, baseline.playlist);
    assert_ne!(reordered.playlist_entries, baseline.playlist_entries);
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4, 1, 3]);
    for command in [
        Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0]],
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![],
            before_entry_id: None,
            expected_revision: token,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.playlist_query_revisions(1), reordered);
        assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4, 1, 3]);
    }
    core.command(Command::RemovePlaylistEntries {
        playlist_id: 1,
        entry_ids: vec![ids[2], ids[0]],
        expected_revision: reordered.playlist_entries.unwrap().revision,
    })
    .unwrap();
    let removed = core.state.playlist_query_revisions(1);
    assert_ne!(removed.playlist, reordered.playlist);
    assert_ne!(removed.playlist_entries, reordered.playlist_entries);
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4]);
    assert!(
        core.command(Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[1]],
            expected_revision: token,
        })
        .is_err()
    );
    assert!(
        core.command(Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[1]],
            before_entry_id: None,
            expected_revision: reordered.playlist_entries.unwrap().revision,
        })
        .is_err()
    );
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4]);
    assert_eq!(core.state.playlist_query_revisions(1), removed);
}

#[test]
fn playlist_location_batches_publish_scoped_tokens_even_for_empty_results() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "mix".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let entry = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    let query = Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 1,
        entry_ids: vec![entry, i64::MAX],
    };
    let before = core.query_response(&query);
    assert_eq!(before.revisions, core.state.playlist_query_revisions(1));
    assert!(
        matches!(before.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.len() == 1 && rows[0].position == 0)
    );
    let empty = core.query_response(&Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 1,
        entry_ids: vec![i64::MAX],
    });
    assert_eq!(empty.revisions, before.revisions);
    assert!(
        matches!(empty.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.is_empty())
    );
    let oversized = core.query_response(&Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 1,
        entry_ids: vec![entry; MAX_QUERY_ROWS + 1],
    });
    assert_eq!(oversized.revisions, before.revisions);
    assert!(matches!(oversized.result, Err(QueryError::Message(_))));
    core.command(Command::AddPlaylist {
        playlist_id: 1,
        track_ids: vec![3],
    })
    .unwrap();
    assert!(
        core.command(Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![entry],
            before_entry_id: None,
            expected_revision: before.revisions.playlist_entries.unwrap().revision,
        })
        .is_err()
    );
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [1, 2, 3]);
    let current = core.query_response(&query);
    assert_ne!(
        current.revisions.playlist_entries,
        before.revisions.playlist_entries
    );
    assert!(
        matches!(current.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows[0].total == 3)
    );
}

#[test]
fn playlist_append_and_move_only_invalidate_changed_pages() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "mix".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let entry_id = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    let expected = core.state.playlist_query_revisions(1);
    let query = Query::PlaylistEntryBatch {
        query: None,
        playlist_id: 1,
        after: None,
        limit: 1,
        expected: Some(expected),
    };
    for command in [
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![],
        },
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![2, 1, 2],
        },
        Command::AddPlaylistSources {
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![],
        },
        Command::AddPlaylistSources {
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![1, 2],
        },
        Command::MovePlaylistEntry { entry_id, index: 0 },
    ] {
        core.command(command).unwrap();
        assert_eq!(core.state.playlist_query_revisions(1), expected);
        assert!(core.view(&query).is_ok());
    }
    for command in [
        Command::AddPlaylist {
            playlist_id: 999,
            track_ids: vec![],
        },
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![3, 999],
        },
        Command::AddPlaylistSources {
            playlist_id: 999,
            directories: vec![],
            track_ids: vec![],
        },
        Command::AddPlaylistSources {
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![3, 999],
        },
        Command::MovePlaylistEntry {
            entry_id,
            index: usize::MAX,
        },
        Command::MovePlaylistEntry {
            entry_id: i64::MAX,
            index: 0,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.playlist_query_revisions(1), expected);
        assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [1, 2]);
    }
    for command in [
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![3],
        },
        Command::AddPlaylistSources {
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![4],
        },
        Command::MovePlaylistEntry { entry_id, index: 3 },
    ] {
        let reordered = matches!(command, Command::MovePlaylistEntry { .. });
        let before = core.state.playlist_query_revisions(1);
        let query = Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 1,
            expected: Some(before),
        };
        core.command(command).unwrap();
        let after = core.state.playlist_query_revisions(1);
        if reordered {
            assert_eq!(after.playlist, before.playlist);
            assert_ne!(after.playlist_entries, before.playlist_entries);
        } else {
            assert_ne!(after.playlist, before.playlist);
        }
        assert_eq!(after.library, before.library);
        assert_eq!(after.queue, before.queue);
        assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
    }
    assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [2, 3, 4, 1]);
}

#[test]
fn directory_missing_query_composes_filters_ancestors_and_exact_pages() {
    use crate::model::DirectoryRow;
    let (directory, core) = fixture_with_database(Some(10.0), true);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "UPDATE tracks SET path='/music/a/deep/missing.wav', raw_title='Needle Nested', favorite=1, missing=1 WHERE id=1;
             UPDATE tracks SET path='/music/a/deep/present.wav', raw_title='Needle Present', favorite=1, missing=0 WHERE id=2;
             UPDATE tracks SET path='/music/b/deep/missing.wav', raw_title='Needle Other', favorite=0, missing=1 WHERE id=3;
             UPDATE tracks SET path='/music/direct.wav', raw_title='Needle Direct', favorite=1, missing=1 WHERE id=4;
             INSERT INTO tracks(id,path,raw_title,favorite,missing) VALUES
             (5,'/music/present/only.wav','Other',0,0),
             (6,'/music/hidden/deep/missing.wav','Other',1,1);",
        )
        .unwrap();
    let page = |path: &str, query: Option<&str>, favorite, missing, offset, limit| {
        let response = core.query_response(&Query::DirectoryPage {
            path: path.into(),
            query: query.map(str::to_owned),
            favorite,
            missing,
            offset,
            limit,
        });
        let Ok(ViewResponse::DirectoryPage(page)) = response.result else {
            panic!("expected directory page");
        };
        page
    };
    let directory_row = |path: &str| DirectoryRow::Directory { path: path.into() };
    for (missing, paths, track_ids) in [
        (true, vec!["/music/a", "/music/b", "/music/hidden"], vec![4]),
        (false, vec!["/music/a", "/music/present"], vec![]),
    ] {
        let all = page("/music", None, None, Some(missing), 0, 10);
        assert_eq!(all.total, paths.len() + track_ids.len());
        assert_eq!(
            &all.rows[..paths.len()],
            paths
                .iter()
                .map(|path| directory_row(path))
                .collect::<Vec<_>>()
        );
        for (row, id) in all.rows[paths.len()..].iter().zip(track_ids) {
            assert!(
                matches!(row, DirectoryRow::Track(row) if row.id == id && row.missing == missing)
            );
        }
        for offset in 0..all.total {
            let single = page("/music", None, None, Some(missing), offset, 1);
            assert_eq!(single.total, all.total);
            assert_eq!(single.rows, all.rows[offset..offset + 1]);
        }
        for (offset, limit) in [(0, 0), (all.total, 10), (usize::MAX, 10)] {
            let empty = page("/music", None, None, Some(missing), offset, limit);
            assert_eq!(empty.total, all.total);
            assert!(empty.rows.is_empty());
        }
    }
    for missing in [true, false] {
        let selected = page("/music", Some("NEEDLE"), Some(true), Some(missing), 0, 10);
        assert_eq!(selected.total, if missing { 2 } else { 1 });
        assert_eq!(selected.rows[0], directory_row("/music/a"));
        if missing {
            assert!(
                matches!(&selected.rows[1], DirectoryRow::Track(row) if row.id == 4 && row.favorite && row.missing)
            );
        }
        let ancestor = page("/music/a", Some("needle"), Some(true), Some(missing), 0, 10);
        assert_eq!(ancestor.total, 1);
        assert_eq!(ancestor.rows, [directory_row("/music/a/deep")]);
        let leaf = page(
            "/music/a/deep",
            Some("needle"),
            Some(true),
            Some(missing),
            0,
            10,
        );
        assert_eq!(leaf.total, 1);
        let expected_id = if missing { 1 } else { 2 };
        assert!(
            matches!(&leaf.rows[..], [DirectoryRow::Track(row)] if row.id == expected_id && row.favorite && row.missing == missing)
        );
        let count = page("/music", Some("needle"), Some(true), Some(missing), 0, 0);
        assert_eq!(count.total, selected.total);
        assert!(count.rows.is_empty());
        let absent = page("/music", Some("absent"), Some(true), Some(missing), 0, 10);
        assert_eq!(absent.total, 0);
        assert!(absent.rows.is_empty());
    }
    let non_favorite = page("/music", Some("needle"), Some(false), Some(true), 0, 10);
    assert_eq!(non_favorite.total, 1);
    assert_eq!(non_favorite.rows, [directory_row("/music/b")]);
    let absent = page("/music/hidden", None, None, Some(false), 0, 10);
    assert_eq!(absent.total, 0);
    assert!(absent.rows.is_empty());
}

#[test]
fn playlist_reorders_preserve_catalog_and_other_playlist_queries() {
    let (_directory, mut core) = fixture();
    for (name, tracks) in [("one", vec![1, 2, 3]), ("two", vec![2, 3, 4])] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids: tracks,
        })
        .unwrap();
    }
    core.publish();
    let before = core.state.revisions;
    let summaries = Query::PlaylistSummaries {
        query: None,
        offset: 0,
        limit: 2,
    };
    let catalog = core.query_response(&summaries);
    let first_entry = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    let old_lookup = core.query_response(&Query::PlaylistEntry {
        entry_id: first_entry,
    });
    let batch = |playlist_id, after, expected| Query::PlaylistEntryBatch {
        query: None,
        playlist_id,
        after,
        limit: 1,
        expected,
    };
    let first = core.query_response(&batch(1, None, Some(catalog.revisions)));
    let other = core.query_response(&batch(2, None, Some(catalog.revisions)));
    let Ok(ViewResponse::PlaylistEntryBatch(entries)) = &first.result else {
        panic!("expected first entry batch");
    };
    let continuation = entries.next;
    let old_snapshot = GuiSnapshot::from_core(&core.state);
    core.command(Command::MovePlaylistEntry {
        entry_id: first_entry,
        index: 2,
    })
    .unwrap();
    core.publish();
    assert_eq!(
        core.state.revisions.changed_since(before),
        StateSections {
            library: true,
            ..StateSections::default()
        }
    );
    assert_eq!(
        serde_json::to_value(core.query_response(&summaries)).unwrap(),
        serde_json::to_value(&catalog).unwrap(),
    );
    assert_eq!(old_snapshot.library.playlist_entry_revisions.len(), 2);
    assert_eq!(core.state.library.playlist_entry_revisions.len(), 2);
    assert_ne!(
        old_snapshot.library.playlist_entry_revisions.get(&1),
        core.state.library.playlist_entry_revisions.get(&1)
    );
    assert_eq!(
        old_snapshot.library.playlist_entry_revisions.get(&2),
        core.state.library.playlist_entry_revisions.get(&2)
    );
    assert_eq!(core.state.query_revisions(), catalog.revisions);
    assert_eq!(
        core.view(&batch(1, continuation, Some(first.revisions)))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(core.view(&batch(2, None, Some(other.revisions))).is_ok());
    // A summary baseline cannot silently continue an entry traversal.
    assert_eq!(
        core.view(&batch(1, continuation, Some(catalog.revisions)))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let fresh = core.query_response(&Query::PlaylistEntry {
        entry_id: first_entry,
    });
    assert_ne!(
        fresh.revisions.playlist_entries,
        old_lookup.revisions.playlist_entries
    );
    assert!(
        matches!(fresh.result, Ok(ViewResponse::PlaylistEntry(Some(entry)))
        if entry.id == first_entry && entry.position == 2)
    );
    assert!(
        core.view(&Query::PlaylistSummaryBatch {
            query: None,
            after: Some(1),
            limit: 1,
            expected: Some(catalog.revisions),
        })
        .is_ok()
    );
    assert!(
        core.view(&Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 1,
            expected: Some(catalog.revisions),
        })
        .is_ok()
    );

    let current = core.state.playlist_query_revisions(1);
    let other_entry = core
        .store
        .playlist_entries_page(2, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    // Two mutations may coalesce into one snapshot; both scoped tokens survive.
    for index in [2, 1] {
        core.command(Command::MovePlaylistEntry {
            entry_id: other_entry,
            index,
        })
        .unwrap();
    }
    core.publish();
    assert!(core.view(&batch(1, None, Some(current))).is_ok());
    assert_eq!(
        core.view(&batch(2, None, Some(other.revisions)))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert_eq!(core.state.library.playlist_entry_revisions.len(), 2);
    assert_eq!(core.state.query_revisions(), catalog.revisions);
    // A token for another playlist is never an entry-order baseline.
    assert_eq!(
        core.view(&batch(2, None, Some(current))).unwrap_err(),
        QueryError::RevisionChanged
    );
}

#[test]
fn playlist_catalog_changes_preserve_unrelated_content_and_metadata_has_its_own_stamp() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "one".into(),
        track_ids: vec![1, 2, 3],
    })
    .unwrap();
    let removed = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    for command in [
        Command::CreatePlaylist { name: "two".into() },
        Command::RenamePlaylist {
            playlist_id: 1,
            name: "renamed".into(),
        },
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![4],
        },
        Command::RemovePlaylistEntry { entry_id: removed },
        Command::DeletePlaylist { playlist_id: 1 },
    ] {
        let page = core.store.playlist_entries_page(1, None, 0, 1).unwrap();
        core.command(Command::MovePlaylistEntry {
            entry_id: page.rows[0].id,
            index: page.total - 1,
        })
        .unwrap();
        let expected = core.state.playlist_query_revisions(1);
        let content_changed = matches!(
            command,
            Command::AddPlaylist { .. }
                | Command::RemovePlaylistEntry { .. }
                | Command::DeletePlaylist { .. }
        );
        core.command(command).unwrap();
        assert_ne!(core.state.library.playlist_revision, expected.playlist);
        assert_eq!(core.state.library.revision, expected.library);
        let current = core.state.playlist_query_revisions(1);
        assert_eq!(
            current.duplicate_members != expected.duplicate_members,
            content_changed
        );
        assert_eq!(
            core.state.library.playlist_entry_revisions.len(),
            core.store.playlist_ids().unwrap().len()
        );
        let query = Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 1,
            expected: Some(expected),
        };
        if content_changed {
            assert_ne!(current.playlist_entries, expected.playlist_entries);
            assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
        } else {
            assert_eq!(current.playlist_entries, expected.playlist_entries);
            assert!(core.view(&query).is_ok());
        }
    }
    core.command(Command::AddPlaylist {
        playlist_id: 2,
        track_ids: vec![1, 2],
    })
    .unwrap();
    let expected = core.state.playlist_query_revisions(2);
    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("edited".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert_ne!(core.state.library.revision, expected.library);
    assert_eq!(
        core.view(&Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 2,
            after: None,
            limit: 1,
            expected: Some(expected),
        })
        .unwrap_err(),
        QueryError::RevisionChanged
    );
    let response = core.query_response(&Query::PlaylistEntries {
        query: None,
        playlist_id: 2,
        offset: 0,
        limit: 1,
    });
    assert!(
        matches!(response.result, Ok(ViewResponse::PlaylistEntries(page))
        if page.rows[0].title == "edited")
    );
}

#[test]
fn targeted_metadata_preserves_unrelated_track_and_playlist_reads() {
    let (_directory, mut core) = fixture();
    for (name, track_ids) in [("one", vec![1, 2]), ("two", vec![3, 4])] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids,
        })
        .unwrap();
    }
    let track = Query::Track { track_id: 3 };
    let track_stamp = track.stamp(core.query_response(&track).revisions);
    let entries = Query::PlaylistEntries {
        query: None,
        playlist_id: 2,
        offset: 0,
        limit: 1,
    };
    let baseline = core.query_response(&entries).revisions;
    let batch = Query::PlaylistEntryBatch {
        query: None,
        playlist_id: 2,
        after: None,
        limit: 1,
        expected: Some(baseline),
    };
    for command in [
        Command::EditTrack {
            track_id: 1,
            title: Some("new title".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        },
        Command::SetFavorite {
            track_ids: vec![1, 1],
            favorite: true,
        },
        Command::ToggleFavorite { track_ids: vec![1] },
    ] {
        core.command(command).unwrap();
        assert_eq!(
            track.stamp(core.query_response(&track).revisions),
            track_stamp
        );
        let ViewResponse::PlaylistEntryBatch(page) = core.view(&batch).unwrap() else {
            panic!("expected playlist batch")
        };
        assert_eq!(page.rows[0].track_id, 3);
    }
    let related = Query::PlaylistEntries {
        query: None,
        playlist_id: 1,
        offset: 0,
        limit: 1,
    };
    let before_decoder = core.query_response(&related).revisions;
    let edited = Query::Track { track_id: 1 };
    let edited_before = edited.stamp(core.query_response(&edited).revisions);
    core.command(Command::EditTrack {
        track_id: 1,
        title: None,
        artist: None,
        album: None,
        decoder_hint: Some(crate::model::DecoderHint::Native),
    })
    .unwrap();
    assert_eq!(
        related.stamp(before_decoder),
        related.stamp(core.query_response(&related).revisions)
    );
    assert_ne!(
        edited_before,
        edited.stamp(core.query_response(&edited).revisions)
    );
    let before_failure = core.state.query_revisions();
    assert!(
        core.command(Command::SetFavorite {
            track_ids: vec![1, -1],
            favorite: true
        })
        .is_err()
    );
    assert_eq!(before_failure, core.state.query_revisions());
    assert_eq!(
        track.stamp(core.query_response(&track).revisions),
        track_stamp
    );
}

#[test]
fn playlist_lifecycles_only_invalidate_member_facts_for_committed_members() {
    for (command, has_members, empty_queue) in [
        (
            Command::CreatePlaylist {
                name: "empty".into(),
            },
            false,
            false,
        ),
        (
            Command::CreatePlaylistWithTracks {
                name: "empty tracks".into(),
                track_ids: vec![],
            },
            false,
            false,
        ),
        (
            Command::CreatePlaylistWithTracks {
                name: "tracks".into(),
                track_ids: vec![1, 2, 1],
            },
            true,
            false,
        ),
        (
            Command::CreatePlaylistFromQueue {
                name: "queue".into(),
            },
            true,
            false,
        ),
        (
            Command::CreatePlaylistFromQueue {
                name: "empty queue".into(),
            },
            false,
            true,
        ),
    ] {
        let (_directory, mut core) = fixture();
        core.command(Command::CreatePlaylistWithTracks {
            name: "unaffected".into(),
            track_ids: vec![3, 4],
        })
        .unwrap();
        if empty_queue {
            core.command(Command::ClearQueue).unwrap();
        }
        let unaffected = core.state.playlist_query_revisions(1);
        let other_query = Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 2,
            expected: Some(unaffected),
        };
        let members_query = Query::DuplicateMembers {
            fingerprint: blake3::hash(b"lifecycle facts").to_hex().to_string(),
            offset: 0,
            limit: 10,
        };
        for invalid in [
            Command::CreatePlaylistWithTracks {
                name: "invalid track".into(),
                track_ids: vec![1, i64::MAX],
            },
            Command::DeletePlaylist {
                playlist_id: i64::MAX,
            },
            Command::RenamePlaylist {
                playlist_id: i64::MAX,
                name: "invalid".into(),
            },
        ] {
            assert!(core.command(invalid).is_err());
            assert_eq!(core.state.playlist_query_revisions(1), unaffected);
            assert_eq!(core.state.library.playlist_total, 1);
            assert!(core.view(&other_query).is_ok());
        }
        let playlist_query = Query::PlaylistEntries {
            query: None,
            playlist_id: 2,
            offset: 0,
            limit: 10,
        };
        let members_before = core.query_response(&members_query);
        assert!(members_before.result.is_ok());
        let before = members_before.revisions;
        core.command(command).unwrap();
        let created = core.query_response(&playlist_query);
        let Ok(ViewResponse::PlaylistEntries(page)) = created.result else {
            panic!("expected committed playlist");
        };
        assert_eq!(page.total != 0, has_members);
        assert!(created.revisions.playlist_entries.is_some());
        assert_eq!(core.state.library.playlist_total, 2);
        for deleting in [false, true] {
            let previous = if deleting {
                core.command(Command::DeletePlaylist { playlist_id: 2 })
                    .unwrap();
                created.revisions
            } else {
                before
            };
            let members_after = core.query_response(&members_query);
            assert!(members_after.result.is_ok());
            let after = members_after.revisions;
            assert_eq!(after.library, previous.library);
            assert_eq!(after.structure, previous.structure);
            assert_eq!(after.queue, previous.queue);
            assert_eq!(after.duplicate_groups, previous.duplicate_groups);
            assert_ne!(after.playlist, previous.playlist);
            assert_eq!(
                after.duplicate_members != previous.duplicate_members,
                has_members
            );
            assert_eq!(
                members_query.stamp(after) != members_query.stamp(previous),
                has_members
            );
            let other = core.query_response(&other_query);
            assert!(other.result.is_ok());
            assert_eq!(
                other.revisions.playlist_entries,
                unaffected.playlist_entries
            );
        }
        let deleted = core.query_response(&playlist_query);
        assert!(deleted.result.is_err());
        assert!(deleted.revisions.playlist_entries.is_none());
        assert_eq!(core.state.library.playlist_total, 1);
    }
}

#[test]
fn batch_continuation_rejects_library_change_but_not_playback_change() {
    let (_directory, mut core) = fixture();
    let expected = core.state.query_revisions();
    let ViewResponse::TrackBatch(first) = core
        .view(&Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 2,
            expected: Some(expected),
        })
        .unwrap()
    else {
        panic!("expected track batch");
    };
    let query = Query::TrackBatch {
        query: None,
        favorite: None,
        missing: None,
        after: first.next,
        limit: 2,
        expected: Some(expected),
    };
    core.state.playback.position = 2.0;
    core.publish();
    let ViewResponse::TrackBatch(last) = core.view(&query).unwrap() else {
        panic!("expected track batch");
    };
    assert_eq!(
        last.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
        [3, 4]
    );
    assert_eq!(last.next, None);
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: true,
    })
    .unwrap();
    assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
}

#[test]
fn batch_errors_distinguish_revision_changes_from_invalid_requests() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "mix".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let expected = core.state.query_revisions();
    let entry_expected = core.state.playlist_query_revisions(1);
    let queries = [
        Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 0,
            expected: Some(expected),
        },
        Query::PlaylistSummaryBatch {
            query: None,
            after: None,
            limit: 0,
            expected: Some(expected),
        },
        Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 0,
            expected: Some(entry_expected),
        },
    ];
    for query in &queries {
        assert!(matches!(core.view(query), Err(QueryError::Message(_))));
    }
    core.command(Command::RenamePlaylist {
        playlist_id: 1,
        name: "renamed".into(),
    })
    .unwrap();
    // A stale relevant dependency wins over the invalid limit. Rename changes
    // only summaries; track and entry rows still report the invalid request.
    assert_eq!(
        core.view(&queries[1]).unwrap_err(),
        QueryError::RevisionChanged
    );
    for query in [&queries[0], &queries[2]] {
        assert!(matches!(core.view(query), Err(QueryError::Message(_))));
    }
    let renamed = core.state.query_revisions();
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: true,
    })
    .unwrap();
    for query in &queries {
        assert_eq!(core.view(query).unwrap_err(), QueryError::RevisionChanged);
    }
    // With a current catalog baseline, stale L alone is irrelevant to summaries.
    assert!(matches!(
        core.view(&Query::PlaylistSummaryBatch {
            query: None,
            after: None,
            limit: 0,
            expected: Some(renamed),
        }),
        Err(QueryError::Message(_))
    ));
    let entry_expected = core.state.playlist_query_revisions(1);
    let entry_id = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::MovePlaylistEntry { entry_id, index: 1 })
        .unwrap();
    // Entry content staleness also takes priority independently of metadata.
    assert_eq!(
        core.view(&Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 0,
            expected: Some(entry_expected),
        })
        .unwrap_err(),
        QueryError::RevisionChanged
    );
}

fn settle_scan(core: &mut Core) -> bool {
    while core.active_scan.is_some() {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        core.scan_event(event);
    }
    core.scan_results.back().unwrap().progress.phase == ScanPhase::Completed
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
                Arc::make_mut(&mut core.state.system.config).scan_min_duration_secs = threshold;
                let paths = if directory_scope {
                    vec![directory.path().to_path_buf()]
                } else {
                    vec![existing.clone(), new.clone()]
                };
                core.scan(paths, force).unwrap();
                // Mutating the live configuration after acceptance must not alter
                // this task, regardless of when its worker reaches the source.
                Arc::make_mut(&mut core.state.system.config).scan_min_duration_secs = 1 - threshold;
                assert!(settle_scan(&mut core));
                let status = core.scan_results.back().unwrap();
                assert_eq!(status.progress.errors, 0);
                assert!(status.error.is_none());
                assert_eq!(
                    core.store.track_id_for_source(&existing, None).unwrap(),
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
                        .track_id_for_source(&new, None)
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
    Arc::make_mut(&mut core.state.system.config).scan_min_duration_secs = 1;
    core.scan(vec![path.clone()], false).unwrap();
    assert!(settle_scan(&mut core));
    let task = core.scan_results.back().unwrap();
    assert_eq!(task.progress.errors, 0);
    assert!(task.error.is_none());
    assert!(
        core.store
            .track_id_for_source(&path, None)
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
            records: vec![ScanRecord {
                path: path.clone(),
                size: known.size,
                modified_ns: known.modified_ns,
                fingerprint: known.fingerprint,
                media,
                cue: None,
            }],
            ..ScanResult::default()
        })
        .unwrap();
    Arc::make_mut(&mut core.state.system.config).scan_min_duration_secs = 1;
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
                .track_id_for_source(&path, Some(1))
                .unwrap()
                .unwrap()
        } else {
            1
        };
        Arc::make_mut(&mut core.state.system.config).scan_min_duration_secs = 3;
        core.command(Command::ImportPlaylist {
            path: path.clone(),
            name: Some("Filtered mix".into()),
        })
        .unwrap();
        Arc::make_mut(&mut core.state.system.config).scan_min_duration_secs = 0;
        assert!(settle_scan(&mut core));
        let task = core.scan_results.back().unwrap();
        assert_eq!(task.progress.errors, 0);
        assert!(task.error.is_none());
        let playlist = core.store.playlist_summary_page(None, 0, 10).unwrap().rows[0].id;
        assert_eq!(
            core.store.playlist_track_ids(playlist, 10).unwrap(),
            [expected]
        );
        let track = core.store.track(expected).unwrap().unwrap();
        assert!(!track.missing);
        if extension == "cue" {
            assert_eq!(track.duration, Some(1.0));
            assert_eq!(track.title, "Updated segment");
            assert!(
                core.store
                    .track_id_for_source(&path, Some(2))
                    .unwrap()
                    .is_none()
            );
        } else {
            assert_eq!(track.duration, Some(2.0));
        }
        assert!(
            core.store
                .track_id_for_source(&new, None)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn scan_start_ack_binds_the_accepted_task() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    let (requests, receiver) = bounded(3);
    let (reply, ack) = bounded(1);
    requests
        .send(Request {
            operation: Operation::Command(Command::Scan {
                paths: vec![path],
                force: false,
            }),
            reply: Some(reply),
        })
        .unwrap();
    drop(requests);
    run_until_disconnected(&mut core, receiver);
    let CoreResponse::Ack(ack) = ack.recv().unwrap() else {
        panic!("Expected ack");
    };
    assert!(ack.ok);
    let id = ack
        .scan_id
        .expect("Accepted scans must expose their identity");
    let ViewResponse::ScanTask(task) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("Expected task");
    };
    assert_eq!(task.id, id);
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
    let known = core
        .store
        .known_source(&core.track(1).unwrap().path)
        .unwrap()
        .unwrap();
    let mut record = ScanRecord {
        path: known.path,
        cue: known.cue,
        size: known.size,
        modified_ns: known.modified_ns,
        fingerprint: Some(blake3::hash(b"same encoded audio").to_hex().to_string()),
        media: known.media.unwrap(),
    };
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records: vec![record.clone()],
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
    assert!(
        core.finish_scan(ScanFinished {
            id: 1,
            result: Ok(ScanResult {
                records: vec![record],
                ..Default::default()
            }),
            import: None,
            progress: ScanProgress {
                phase: ScanPhase::Saving,
                processed: 1,
                total: Some(1),
                errors: 0,
                path: None
            },
        })
        .unwrap()
    );
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
            if matches!(event, ScanEvent::KnownSource { .. }) {
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
            event @ ScanEvent::KnownSource { .. } => core.scan_event(event),
            ScanEvent::Finished(scan) => break scan,
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
        id: 0,
        result: Ok(ScanResult::default()),
        import: None,
        progress,
    }));
    let (reply, receive) = bounded(1);
    core.scan_event(ScanEvent::KnownSource {
        id: 0,
        path: directory.path().join("1.wav"),
        reply,
    });
    assert!(receive.recv().unwrap().is_err());
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

fn write_pcm(path: &Path) {
    write_pcm_frames(path, 1);
}

fn write_pcm_frames(path: &Path, frames: u32) {
    use std::io::Write;
    let data_len = frames * 200;
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16_u32.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&7500_u32.to_le_bytes()).unwrap();
    file.write_all(&15000_u32.to_le_bytes()).unwrap();
    file.write_all(&2_u16.to_le_bytes()).unwrap();
    file.write_all(&16_u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_len.to_le_bytes()).unwrap();
    file.write_all(&vec![0; data_len as usize]).unwrap();
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
    let mut config = core.state.system.config.as_ref().clone();
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
        .track_id_for_source(&hidden, None)
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
    let mut config = core.state.system.config.as_ref().clone();
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
        core.state
            .system
            .last_error
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
        .track_id_for_source(&visible, None)
        .unwrap()
        .unwrap();
    assert!(!core.track(visible_id).unwrap().missing);
    let mut config = core.state.system.config.as_ref().clone();
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
    Arc::make_mut(&mut core.state.system.config)
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
    Arc::make_mut(&mut core.state.system.config).library_roots = vec![root];
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
        .track_id_for_source(&logical, None)
        .unwrap()
        .unwrap();
    assert_eq!(core.track(id).unwrap().path, logical);
    assert!(
        core.store
            .track_id_for_source(&source, None)
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
    let mut config = core.state.system.config.as_ref().clone();
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
            .track_id_for_source(&path, None)
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
    Arc::make_mut(&mut core.state.system.config)
        .library_roots
        .push(directory.path().join("offline/Music"));
    core.command(Command::Scan {
        paths: vec![source.clone()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    assert_eq!(core.track(1).unwrap().path, source);
    assert!(!core.track(1).unwrap().missing);
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
    Arc::make_mut(&mut core.state.system.config).library_roots = vec![sheets, sources];
    core.command(Command::Scan {
        paths: vec![sheet.clone()],
        force: false,
    })
    .unwrap();
    assert!(settle_scan(&mut core));
    let id = core
        .store
        .track_id_for_source(&sheet, Some(1))
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
    Arc::make_mut(&mut core.state.system.config).library_roots = vec![root.clone()];
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
    Arc::make_mut(&mut core.state.system.config)
        .library_roots
        .push(missing.clone());
    core.command(Command::Scan {
        paths: vec![missing, broken, source.clone()],
        force: false,
    })
    .unwrap();
    assert!(!settle_scan(&mut core));
    assert!(core.state.system.last_error.is_some());
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
    let mut config = core.state.system.config.as_ref().clone();
    config.library_roots = vec![alias.join("unused/.."), outside.path().to_path_buf()];
    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap();
    assert_eq!(
        core.state.system.config.library_roots,
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
        .track_id_for_source(&source, None)
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
        .track_id_for_source(&logical, None)
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

fn pending(core: &mut Core, queue_id: u64) {
    core.store.set_queue_current(Some(queue_id)).unwrap();

    core.refresh_queue_tracks().unwrap();
    core.state.playback.status = PlaybackStatus::Playing;
    let (track_id, duration) = {
        let track = core.state.current_track().unwrap();
        (track.id, track.duration)
    };
    core.state.playback.duration = duration;
    core.playback = Some(PlaybackStats {
        track_id,
        source: core
            .playback_source(core.state.current_track().unwrap())
            .unwrap(),
        heard: 0.0,
        position: None,
        started: false,
        counted: false,
        last_heard_at: None,
        activity_updated: false,
        paused_at: None,
    });
}

fn started_event(core: &Core, duration: Option<f64>) -> AudioEvent {
    let track = core.state.current_track().unwrap();
    AudioEvent::Started {
        generation: core.generation,
        info: MediaInfo {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            duration,
            codec: track.codec.clone(),
            channels: track.channels,
            sample_rate: track.sample_rate,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
        },
    }
}

fn started(core: &mut Core, duration: Option<f64>) -> Result<()> {
    core.audio_event(started_event(core, duration))
}

fn playing(core: &mut Core, queue_id: u64) {
    pending(core, queue_id);
    started(core, Some(10.0)).unwrap();
}

fn progress(core: &mut Core, position_seconds: f64, listened_seconds: f64) {
    core.audio_event(AudioEvent::Progress {
        generation: core.generation,
        position_seconds,
        listened_seconds,
    })
    .unwrap();
}

fn play_count(core: &Core, track_id: i64) -> u64 {
    core.store.track(track_id).unwrap().unwrap().play_count
}

fn missing(core: &mut Core, command: Command, track_id: i64) {
    let path = core.track(track_id).unwrap().path.clone();
    let error = core.command(command).unwrap_err();
    assert!(format!("{error:#}").contains(&path.display().to_string()));
}

fn precision_pending(core: &mut Core) {
    let track = core.store.track(1).unwrap().unwrap();
    std::fs::write(&track.path, b"encoded source").unwrap();
    let metadata = std::fs::metadata(&track.path).unwrap();
    let mut media = core
        .store
        .known_source(&track.path)
        .unwrap()
        .unwrap()
        .media
        .unwrap();
    media.bitrate_bps = Some(128_000);
    media.bitrate_exact = false;
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records: vec![ScanRecord {
                path: track.path,
                cue: None,
                size: metadata.len(),
                modified_ns: metadata
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64,
                fingerprint: None,
                media,
            }],
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    core.store.clear_queue().unwrap();
    core.reload_queue().unwrap();
    core.enqueue(&[1]).unwrap();
    let queue_id = core.queue_ids[0];
    pending(core, queue_id);
    assert!(core.playback.as_ref().unwrap().source.is_some());
}

#[test]
fn playback_started_upgrades_estimate_and_duplicate_cannot_downgrade() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut event {
        info.bitrate_bps = Some(128_000);
        info.bitrate_exact = true;
    }
    let before = core.state.library.revision;
    core.audio_event(event).unwrap();
    assert!(core.store.track(1).unwrap().unwrap().bitrate_exact);
    assert!(core.state.current_track().unwrap().bitrate_exact);
    assert!(
        core.store.queue_page(None, 0, 10).unwrap().rows[0]
            .track
            .bitrate_exact
    );
    // The precision update shares Started's existing content revision.
    assert_eq!(core.state.library.revision, before.wrapping_add(1));
    let revision = core.state.library.revision;
    let mut duplicate = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut duplicate {
        info.bitrate_bps = Some(64_000);
    }
    core.audio_event(duplicate).unwrap();
    assert_eq!(core.state.library.revision, revision);
    assert_eq!(
        core.store.track(1).unwrap().unwrap().bitrate_bps,
        Some(128_000)
    );
}

#[test]
fn stale_playback_start_cannot_upgrade_precision() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    let revision = core.state.library.revision;
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { generation, info } = &mut event {
        *generation = generation.wrapping_sub(1);
        info.bitrate_bps = Some(192_000);
        info.bitrate_exact = true;
    }
    core.audio_event(event).unwrap();
    assert_eq!(core.state.library.revision, revision);
    assert!(!core.playback.as_ref().unwrap().started);
    assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
    assert_eq!(
        core.store.track(1).unwrap().unwrap().bitrate_bps,
        Some(128_000)
    );
}

#[test]
fn full_eof_upgrades_estimate_but_stale_and_stopped_events_do_not() {
    for drain in [false, true] {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        started(&mut core, Some(10.0)).unwrap();
        let generation = core.generation;
        core.audio_event(AudioEvent::Ended {
            generation: generation.wrapping_sub(1),
            bitrate_bps: Some(192_000),
        })
        .unwrap();
        assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
        if drain {
            let (send, receive) = bounded(1);
            core.engine.events = receive;
            send.send(AudioEvent::Ended {
                generation,
                bitrate_bps: Some(192_000),
            })
            .unwrap();
            core.finish_playback(Ok(None), false, Ok(())).unwrap();
        } else {
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: Some(192_000),
            })
            .unwrap();
        }
        let exact = core.store.track(1).unwrap().unwrap();
        assert_eq!(exact.bitrate_bps, Some(192_000));
        assert!(exact.bitrate_exact);
        let revision = core.state.library.revision;
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: Some(256_000),
        })
        .unwrap();
        assert_eq!(core.state.library.revision, revision);
        assert_eq!(core.store.track(1).unwrap().unwrap(), exact);
    }
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    started(&mut core, Some(10.0)).unwrap();
    let generation = core.generation;
    core.finish_playback(Ok(None), false, Ok(())).unwrap();
    core.audio_event(AudioEvent::Ended {
        generation,
        bitrate_bps: Some(192_000),
    })
    .unwrap();
    assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
}

#[test]
fn same_track_restart_retains_queued_started_and_eof_bitrate_upgrades() {
    for eof in [false, true] {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        let queue_id = core.queue_ids[0];
        let generation = core.generation;
        let event = if eof {
            started(&mut core, Some(10.0)).unwrap();
            AudioEvent::Ended {
                generation,
                bitrate_bps: Some(192_000),
            }
        } else {
            let mut event = started_event(&core, Some(10.0));
            if let AudioEvent::Started { info, .. } = &mut event {
                info.bitrate_bps = Some(192_000);
                info.bitrate_exact = true;
            }
            event
        };
        let _commands = recording_audio(&mut core);
        let (sender, events) = bounded(1);
        core.engine.events = events;
        sender.send(event).unwrap();

        core.start(queue_id, false).unwrap();
        assert_eq!(core.generation, generation.wrapping_add(1));
        assert!(!core.playback.as_ref().unwrap().started);
        assert!(core.playback.as_ref().unwrap().source.is_none());

        // A new decoder estimate must not replace the retiring worker's upgrade.
        let mut event = started_event(&core, Some(10.0));
        if let AudioEvent::Started { info, .. } = &mut event {
            info.bitrate_bps = Some(64_000);
        }
        for event in [None, Some(event)] {
            if let Some(event) = event {
                core.audio_event(event).unwrap();
            }
            core.publish();
            let stored = core.store.track(1).unwrap().unwrap();
            assert_eq!(stored.bitrate_bps, Some(192_000));
            assert!(stored.bitrate_exact);
            let state = core.shared.read();
            assert_eq!(state.playback.status, PlaybackStatus::Playing);
            assert_eq!(state.queue.current_id, Some(queue_id));
            assert_eq!(state.current_track().unwrap(), &stored);
            let page = core.store.queue_page(None, 0, 1).unwrap();
            assert_eq!(page.rows[0].track.bitrate_bps, stored.bitrate_bps);
            assert_eq!(page.rows[0].track.bitrate_exact, stored.bitrate_exact);
        }
    }
}

#[test]
fn playback_bitrate_runtime_replacement_and_optional_failure_do_not_interrupt_start() {
    for replace in [false, true] {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        if replace {
            let path = core.state.current_track().unwrap().path.clone();
            std::fs::write(path, b"different encoded source with different size").unwrap();
        }
        let mut event = started_event(&core, Some(10.0));
        if let AudioEvent::Started { info, .. } = &mut event {
            info.bitrate_bps = Some(if replace { 192_000 } else { u64::MAX });
            info.bitrate_exact = true;
        }
        core.audio_event(event).unwrap();
        assert!(core.playback.as_ref().unwrap().started);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        let track = core.store.track(1).unwrap().unwrap();
        assert_eq!(track.bitrate_bps, Some(128_000));
        assert!(!track.bitrate_exact);
    }
}

#[test]
fn successful_start_records_recent_once_and_twenty_percent_is_strict() {
    let (_directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.system.config).play_count_threshold_percent = 20.0;
    pending(&mut core, 1);
    assert!(core.store.history(200).unwrap().is_empty());
    started(&mut core, Some(10.0)).unwrap();
    assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
    assert_eq!(play_count(&core, 1), 0);

    // A duplicate Started must not refresh last-played a second time.
    core.store.mark_played(1, 7).unwrap();
    started(&mut core, Some(10.0)).unwrap();
    assert_eq!(core.store.history(200).unwrap()[0].played_at, 7);
    progress(&mut core, 9.0, 2.0);
    progress(&mut core, 9.0, f64::NAN);
    progress(&mut core, 9.0, f64::INFINITY);
    assert_eq!(play_count(&core, 1), 0);
    progress(&mut core, 9.0, 2.000_001);
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.state.current_track().unwrap().play_count, 1);
    assert_eq!(
        core.store.queue_page(None, 0, 1).unwrap().rows[0]
            .track
            .play_count,
        1
    );
    progress(&mut core, 9.5, 3.0);
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.state.current_track().unwrap().play_count, 1);
    assert_eq!(
        core.store.queue_page(None, 0, 1).unwrap().rows[0]
            .track
            .play_count,
        1
    );
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
            suppressed_sources: Vec::new(),
            skipped_directories: Vec::new(),
            skipped_directories_truncated: false,
            scan_incomplete: true,
            uncertainties: Vec::new(),
            records: vec![ScanRecord {
                path: known.path,
                cue: known.cue,
                size: known.size,
                modified_ns: known.modified_ns + 1,
                fingerprint: known.fingerprint,
                media,
            }],
            errors: Vec::new(),
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    let current = core.state.current_track().unwrap();
    assert_eq!(current.codec, "updated-codec");
    assert_eq!(current.sample_rate, 96_000);
}

#[test]
fn configured_threshold_controls_counting_including_zero_percent() {
    for percent in [0.0, 50.0, 99.0] {
        let (_directory, mut core) = fixture();
        Arc::make_mut(&mut core.state.system.config).play_count_threshold_percent = percent;
        playing(&mut core, 1);
        let boundary = 10.0 * percent / 100.0;
        progress(&mut core, 10.0, boundary);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, 10.0, boundary + 0.000_001);
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn repeated_progress_pause_seek_and_resume_count_only_once() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.store.mark_played(1, 7).unwrap();
    progress(&mut core, 1.0, 1.0);
    core.command(Command::Pause).unwrap();
    progress(&mut core, 1.0, 1.0);
    core.command(Command::Seek { seconds: 9.0 }).unwrap();
    assert_eq!(play_count(&core, 1), 0);
    core.command(Command::Resume).unwrap();
    progress(&mut core, 9.5, 1.5);
    core.command(Command::Seek { seconds: 0.0 }).unwrap();
    progress(&mut core, 0.5, 2.0);
    assert_eq!(play_count(&core, 1), 0);
    progress(&mut core, 1.0, 2.5);
    for _ in 0..3 {
        core.command(Command::Pause).unwrap();
        core.command(Command::Resume).unwrap();
        core.command(Command::Seek { seconds: 5.0 }).unwrap();
        progress(&mut core, 5.0, 2.0);
        progress(&mut core, 8.0, 8.0);
    }
    let final_activity = core.state.playback.last_heard_at.unwrap();
    core.stop().unwrap();
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(
        core.store.history(200).unwrap()[0].played_at,
        final_activity
    );
}

#[test]
fn paused_confirmations_do_not_advance_activity_at_stop() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 2.0, 2.0);
    let heard_at = core.state.playback.last_heard_at;
    core.command(Command::Pause).unwrap();
    let pause_bound = core.playback.as_ref().unwrap().paused_at.unwrap();
    progress(&mut core, 20.0, 20.0);
    assert!(core.state.playback.last_heard_at.unwrap() <= pause_bound);
    assert_eq!(core.playback.as_ref().unwrap().heard, 20.0);
    assert_eq!(play_count(&core, 1), 1);
    let generation = core.generation;
    core.finish_playback(Ok(Some((generation, 20.0))), false, Ok(()))
        .unwrap();
    assert_eq!(
        core.store.history(200).unwrap()[0].played_at,
        core.state
            .playback
            .last_heard_at
            .unwrap_or(heard_at.unwrap())
    );
}

#[test]
fn failed_load_and_stale_events_neither_count_nor_record_recent() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let generation = core.generation;
    let track = core.state.current_track().unwrap();
    core.audio_event(AudioEvent::Started {
        generation: generation.wrapping_sub(1),
        info: MediaInfo {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            duration: Some(10.0),
            codec: track.codec.clone(),
            channels: track.channels,
            sample_rate: track.sample_rate,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
        },
    })
    .unwrap();
    progress(&mut core, 10.0, 10.0);
    core.audio_event(AudioEvent::Failed {
        generation,
        message: "Decoder failed".into(),
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(
        core.state.system.last_error.as_deref(),
        Some("Decoder failed")
    );
    assert_ne!(core.generation, generation);
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 0.0);
    assert_eq!(play_count(&core, 1), 0);
    assert!(core.store.history(200).unwrap().is_empty());
}

#[test]
fn final_stop_snapshot_counts_only_matching_generation_above_boundary() {
    for (heard, stale, expected) in [(2.0, false, 0), (2.01, false, 1), (10.0, true, 0)] {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        progress(&mut core, 2.0, 2.0);
        let generation = core.generation;
        let snapshot_generation = if stale { generation - 1 } else { generation };
        core.finish_playback(Ok(Some((snapshot_generation, heard))), false, Ok(()))
            .unwrap();
        assert_eq!(play_count(&core, 1), expected);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_ne!(core.generation, generation);
        assert!(core.playback.is_none());
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(play_count(&core, 1), expected);
    }
}

#[test]
fn stop_ack_records_queued_start_and_snapshot_without_terminal_navigation() {
    for (heard, expected) in [(2.0, 0), (2.01, 1)] {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        let (sender, events) = bounded(8);
        core.engine.events = events;
        sender.send(started_event(&core, Some(10.0))).unwrap();
        sender.send(started_event(&core, Some(10.0))).unwrap();
        sender
            .send(AudioEvent::Progress {
                generation,
                position_seconds: 9.0,
                listened_seconds: 2.0,
            })
            .unwrap();
        sender
            .send(AudioEvent::Progress {
                generation: generation - 1,
                position_seconds: 10.0,
                listened_seconds: 10.0,
            })
            .unwrap();
        sender
            .send(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
        sender
            .send(AudioEvent::Failed {
                generation,
                message: "Retired failure".into(),
            })
            .unwrap();
        sender
            .send(AudioEvent::DecoderStopped { generation })
            .unwrap();
        core.finish_playback(Ok(Some((generation, heard))), false, Ok(()))
            .unwrap();
        assert_eq!(play_count(&core, 1), expected);
        assert_eq!(core.store.history(200).unwrap().len(), 1);
        assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.queue.current_id, Some(1));
        assert_eq!(core.state.playback.position, 0.0);
        assert!(core.state.system.last_error.is_none());
        assert_eq!(core.generation, generation.wrapping_add(1));
        assert!(core.playback.is_none());
    }
}

#[test]
fn queued_start_rejection_fences_once_without_recursive_stop_or_count() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let generation = core.generation;
    let (sender, events) = bounded(4);
    core.engine.events = events;
    sender.send(started_event(&core, Some(10.0))).unwrap();
    sender
        .send(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
    sender
        .send(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
    core.store.remove_tracks(&[1]).unwrap();
    assert!(
        core.finish_playback(Ok(Some((generation, 10.0))), false, Ok(()))
            .is_err()
    );
    assert_eq!(core.generation, generation.wrapping_add(1));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.playback.is_none());
    while let Ok(event) = core.engine.events.try_recv() {
        core.audio_event(event).unwrap();
    }
    assert!(core.store.history(200).unwrap().is_empty());
    assert_eq!(play_count(&core, 2), 0);
}

#[test]
fn database_optimization_requires_stopped_playback_and_no_scan() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.system.database_optimization = Some(DatabaseOptimization {
        database_bytes_before: 1,
        database_bytes_after: 1,
        wal_bytes_before: 1,
        wal_bytes_after: 0,
    });
    assert!(core.command(Command::OptimizeDatabase).is_err());
    assert!(core.state.system.database_optimization.is_none());
    core.command(Command::Pause).unwrap();
    assert!(core.command(Command::OptimizeDatabase).is_err());
    core.stop().unwrap();
    core.state.system.scanning = true;
    assert!(core.command(Command::OptimizeDatabase).is_err());
    core.state.system.scanning = false;
    core.enqueue(&[4]).unwrap();
    core.command(Command::OptimizeDatabase).unwrap();
    assert!(core.state.system.database_optimization.is_some());
    assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
    assert_eq!(play_count(&core, 1), 0);
}

#[test]
fn favorite_updates_preserve_counts_and_recent_history() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 3.0, 3.0);
    let played_at = core.store.history(200).unwrap()[0].played_at;
    core.command(Command::SetFavorite {
        track_ids: vec![1, 2],
        favorite: true,
    })
    .unwrap();
    assert!(core.track(1).unwrap().favorite);
    assert!(core.track(2).unwrap().favorite);
    assert!(!core.track(3).unwrap().favorite);
    assert!(core.store.history(200).unwrap()[0].favorite);
    assert!(core.state.library.history[0].favorite);
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: false,
    })
    .unwrap();
    assert!(!core.track(1).unwrap().favorite);
    assert!(core.track(2).unwrap().favorite);
    assert!(!core.store.history(200).unwrap()[0].favorite);
    assert!(!core.state.library.history[0].favorite);
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.store.history(200).unwrap()[0].played_at, played_at);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
}

#[test]
fn favorite_toggle_inverts_mixed_tracks_once_and_rejects_invalid_groups() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 3.0, 3.0);
    core.command(Command::SetFavorite {
        track_ids: vec![2],
        favorite: true,
    })
    .unwrap();
    let playback = core.state.playback.clone();
    let played_at = core.store.history(200).unwrap()[0].played_at;
    let revisions = core.state.query_revisions();
    for track_ids in [vec![], vec![1, 999]] {
        let empty = track_ids.is_empty();
        assert_eq!(
            core.command(Command::ToggleFavorite { track_ids }).is_ok(),
            empty
        );
        assert_eq!(core.state.query_revisions(), revisions);
        assert!(!core.track(1).unwrap().favorite);
        assert!(core.track(2).unwrap().favorite);
    }
    for (first, second) in [(true, false), (false, true)] {
        core.command(Command::ToggleFavorite {
            track_ids: vec![1, 2, 1],
        })
        .unwrap();
        assert_eq!(core.track(1).unwrap().favorite, first);
        assert_eq!(core.track(2).unwrap().favorite, second);
        assert!(!core.track(3).unwrap().favorite);
        assert_eq!(core.state.current_track().unwrap().favorite, first);
        assert_eq!(core.state.library.history[0].favorite, first);
        let history = core.store.history(200).unwrap();
        assert_eq!(history[0].favorite, first);
        assert_eq!(history[0].played_at, played_at);
        for row in core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap().rows {
            let expected = match row.track_id {
                1 => first,
                2 => second,
                _ => false,
            };
            assert_eq!(row.track.favorite, expected);
        }
        assert_ne!(core.state.query_revisions().library, revisions.library);
        assert_ne!(core.state.query_revisions().queue, revisions.queue);
        assert_eq!(core.state.playback, playback);
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn removing_missing_tracks_reuses_queue_playlist_and_recent_cleanup_without_disk_deletes() {
    let (_directory, mut core) = fixture();
    let kept_path = core.track(1).unwrap().path.clone();
    std::fs::write(&kept_path, b"Do not delete").unwrap();
    let playlist_id = core.store.create_playlist("Mixed availability").unwrap();
    core.store.add_playlist(playlist_id, &[1, 2]).unwrap();
    core.playlist_created(playlist_id, true).unwrap();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            roots: vec![core.track(2).unwrap().path.clone()],
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    playing(&mut core, 2);
    core.played = vec![1, 2, 3];
    core.played_cursor = 2;
    core.command(Command::RemoveMissingTracks).unwrap();
    assert_eq!(
        core.store
            .library_view_page(None, None, None, LibrarySort::Id, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        [1, 3, 4]
    );
    assert_eq!(core.queue_ids, [1, 4, 5]);
    assert_eq!(
        core.store
            .playlist_entries_page(playlist_id, None, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(core.played, [1]);
    assert_eq!(core.played_cursor, 1);
    assert_eq!(core.state.queue.current_id, None);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.state.library.history.is_empty());
    assert_eq!(std::fs::read(&kept_path).unwrap(), b"Do not delete");
}

#[test]
fn forced_scan_reprobes_unchanged_files_instead_of_reusing_cached_metadata() {
    let (_directory, mut core) = fixture();
    let path = core.track(1).unwrap().path.clone();
    write_pcm(&path);
    let mut scanned = library::scan_paths(
        std::slice::from_ref(&path),
        |_| Ok(None),
        false,
        &[_directory.path().to_path_buf()],
        core.state.system.config.scan_max_depth,
        &library::ScanCancellation::default(),
        |_, _, _, _, _| {},
    )
    .unwrap();
    let probed_title = scanned.records[0].media.title.clone();
    scanned.records[0].media.title = "Cached metadata".into();
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
fn stale_events_cannot_count_or_stop_new_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let generation = core.generation;
    core.stop().unwrap();
    playing(&mut core, 2);
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    core.audio_event(AudioEvent::Ended {
        generation,
        bitrate_bps: None,
    })
    .unwrap();
    core.audio_event(AudioEvent::Failed {
        generation,
        message: "Old failure".into(),
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.state.playback.position, 0.0);
    assert!(core.state.system.last_error.is_none());
    assert_eq!(play_count(&core, 1), 0);
    assert_eq!(play_count(&core, 2), 0);
    progress(&mut core, 3.0, 3.0);
    assert_eq!(play_count(&core, 2), 1);
}

#[test]
fn started_uses_valid_decoder_duration_or_keeps_known_metadata_duration() {
    for duration in [
        None,
        Some(f64::NAN),
        Some(0.0),
        Some(f64::INFINITY),
        Some(5.0),
    ] {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        started(&mut core, duration).unwrap();
        let expected_duration = if duration == Some(5.0) { 5.0 } else { 10.0 };
        assert_eq!(core.state.playback.duration, Some(expected_duration));
        let boundary = expected_duration * 20.0 / 100.0;
        progress(&mut core, expected_duration, boundary);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, expected_duration, boundary + 0.000_001);
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn rejected_recent_update_stops_without_counting_the_start() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let generation = core.generation;
    core.store.remove_tracks(&[1]).unwrap();
    assert!(started(&mut core, Some(10.0)).is_err());
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_ne!(core.generation, generation);
    assert!(core.playback.is_none());
    assert!(core.store.history(200).unwrap().is_empty());
    assert_eq!(play_count(&core, 2), 0);
}

#[test]
fn unknown_duration_waits_for_natural_eof_and_uses_confirmed_position() {
    for (heard, expected) in [(2.0, 0), (2.01, 1), (8.0, 1)] {
        let (_directory, mut core) = fixture_with_duration(None);
        pending(&mut core, 1);
        started(&mut core, None).unwrap();
        progress(&mut core, 10.0, heard);
        assert_eq!(play_count(&core, 1), 0);
        let generation = core.generation;
        assert!(
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None
            })
            .is_err()
        );
        assert_eq!(play_count(&core, 1), expected);
        assert_eq!(core.state.playback.duration, Some(10.0));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    }
}

#[test]
fn unknown_duration_never_uses_seek_targets_heard_time_or_manual_stop() {
    let (_directory, mut core) = fixture_with_duration(None);
    pending(&mut core, 1);
    started(&mut core, None).unwrap();
    core.command(Command::Seek { seconds: 10.0 }).unwrap();
    progress(&mut core, f64::NAN, 8.0);
    let generation = core.generation;
    assert!(
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None
        })
        .is_err()
    );
    assert_eq!(play_count(&core, 1), 0);
    assert_eq!(core.state.playback.duration, None);

    pending(&mut core, 1);
    started(&mut core, None).unwrap();
    progress(&mut core, 10.0, 8.0);
    core.stop().unwrap();
    assert_eq!(play_count(&core, 1), 0);
}

#[test]
fn stopped_decoder_final_progress_counts_once_and_fences_late_events() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 2.0, 2.0);
    progress(&mut core, 2.1, 2.1);
    let generation = core.generation;
    core.audio_event(AudioEvent::DecoderStopped { generation })
        .unwrap();
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.playback.position, 0.0);
    assert!(core.playback.is_none());
    assert_eq!(play_count(&core, 1), 1);
}

#[test]
fn m3u_playlist_deduplicates_in_first_occurrence_order_without_deduplicating_queue() {
    let (directory, mut core) = fixture();
    for track_id in [1, 2, 3] {
        write_pcm(&core.track(track_id).unwrap().path);
    }
    let path = directory.path().join("duplicates.m3u");
    std::fs::write(&path, "2.wav\n1.wav\n2.wav\n3.wav\n1.wav\n").unwrap();
    core.command(Command::ImportPlaylist {
        path,
        name: Some("First occurrences".into()),
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
        [2, 1, 3]
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
fn cue_playlist_import_deduplicates_segments_in_first_occurrence_order() {
    let (directory, mut core) = fixture();
    let sheet = directory.path().join("album.cue");
    std::fs::write(&sheet, "FILE album.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:05:00\n").unwrap();
    let items = library::import_playlist(&sheet).unwrap();
    let records = (1..=2)
        .map(|number| ScanRecord {
            path: directory.path().join("album.wav"),
            cue: Some(CueSegment {
                sheet: sheet.clone(),
                number,
                start_frame: u64::from(number - 1) * 375,
                end_frame: if number == 1 { Some(375) } else { None },
            }),
            size: 1,
            modified_ns: 1,
            fingerprint: None,
            media: MediaInfo {
                title: format!("Part {number}"),
                artist: String::new(),
                album: "Album".into(),
                duration: Some(5.0),
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
        })
        .collect();
    core.finish_scan(ScanFinished {
        id: 0,
        result: Ok(ScanResult {
            roots: vec![sheet],
            records,
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
    })
    .unwrap();
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
        ["Part 2", "Part 1"]
    );
    assert_ne!(tracks[0].id, tracks[1].id);
    assert_ne!(entries[0].id, entries[1].id);
}

#[test]
fn ended_missing_next_stops_and_resume_attempts_a_load() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.playback.position = 10.0;
    let generation = core.generation;
    let error = core
        .audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("2.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.playback.position, 0.0);
    assert!(core.playback.is_none());
    assert_eq!(core.store.history(1).unwrap()[0].track_id, 1);
    assert_ne!(core.generation, generation);
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 0.0);
    // Resume must try to reopen the selected track, not send Pause(false)
    // to an empty worker and falsely report playing.
    missing(&mut core, Command::Resume, 1);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
}

#[test]
fn missing_manual_next_preserves_current_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.playback.position = 4.0;
    let generation = core.generation;
    let played_at = core.store.history(1).unwrap()[0].played_at;
    missing(&mut core, Command::Next, 2);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(core.state.playback.position, 4.0);
    assert_eq!(core.generation, generation);
    assert_eq!(core.playback.as_ref().unwrap().track_id, 1);
    assert_eq!(core.store.history(1).unwrap()[0].played_at, played_at);
}

#[test]
fn count_save_failure_still_stops_and_preserves_audio_error() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let playback = core.playback.as_mut().unwrap();
    playback.track_id = i64::MAX;
    playback.heard = 3.0;
    let generation = core.generation;
    let error = core
        .audio_event(AudioEvent::Failed {
            generation,
            message: "Decoder failed".into(),
        })
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("Decoder failed"));
    assert!(message.contains("does not exist"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_ne!(core.generation, generation);
}

#[test]
fn album_commands_resolve_all_members_and_reject_stale_keys_before_stop() {
    let (directory, mut core) = fixture();
    let records = (0..MAX_QUERY_ROWS + 3)
        .map(|number| ScanRecord {
            path: directory.path().join(format!("album-{number}.wav")),
            cue: None,
            size: 1,
            modified_ns: 1,
            fingerprint: None,
            media: MediaInfo {
                title: format!("Track {number}"),
                artist: String::new(),
                album: "Large".into(),
                duration: Some(10.0),
                codec: "pcm".into(),
                channels: 2,
                sample_rate: 48_000,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: Some(number as u32 + 1),
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            },
        })
        .collect();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            roots: vec![directory.path().into()],
            records,
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    let key = crate::model::AlbumKey {
        directory: directory.path().into(),
        title: "Large".into(),
    };
    core.command(Command::EnqueueAlbum { key: key.clone() })
        .unwrap();
    assert_eq!(core.queue_ids.len(), MAX_QUERY_ROWS + 8);
    let first = core.store.album_tracks_page(&key, None, 0, 1).unwrap().rows[0].id;
    write_pcm(&core.track(first).unwrap().path);
    let _commands = recording_audio(&mut core);
    core.command(Command::PlayAlbum { key }).unwrap();
    assert_eq!(core.queue_ids.len(), MAX_QUERY_ROWS + 3);
    assert_eq!(core.store.queue_track_id(core.queue_ids[0]).unwrap(), first);
    assert_eq!(core.state.queue.current_id, Some(core.queue_ids[0]));
    let queue = core.queue_ids.clone();
    let generation = core.generation;
    let current_id = core.state.queue.current_id;
    let playback_status = core.state.playback.status;
    for title in ["Absent", "", " \t\r\n", "\u{a0}\u{2003}\u{202f}\u{3000}"] {
        let key = crate::model::AlbumKey {
            directory: directory.path().into(),
            title: title.into(),
        };
        for command in [
            Command::PlayAlbum { key: key.clone() },
            Command::EnqueueAlbum { key: key.clone() },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(core.queue_ids, queue);
            assert_eq!(core.generation, generation);
            assert_eq!(core.state.queue.current_id, current_id);
            assert_eq!(core.state.playback.status, playback_status);
        }
    }
}

fn queue_entries(core: &Core) -> Vec<(u64, i64)> {
    core.store
        .queue_page(None, 0, MAX_QUERY_ROWS)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| (row.id, row.track_id))
        .collect()
}

fn assert_queue_saved(core: &Core) {
    assert_eq!(core.store.queue_ids().unwrap(), core.queue_ids);
    assert_eq!(core.store.queue_state().unwrap(), core.state.queue);
}

#[test]
fn single_queue_removal_survives_restore() {
    let (_directory, mut core) = fixture();
    core.state.queue.current_id = Some(2);
    core.save_queue().unwrap();
    core.command(Command::RemoveQueue { queue_id: 2 }).unwrap();
    core.load_initial_state().unwrap();
    assert_eq!(queue_entries(&core), [(1, 1), (3, 2), (4, 3), (5, 4)]);
    assert_eq!(core.state.queue.current_id, None);
}

#[test]
fn playlist_creation_failure_leaves_catalog_and_versions_unchanged() {
    let (_directory, mut core) = fixture();
    core.publish();
    let before = core.state.query_revisions();
    assert!(
        core.command(Command::CreatePlaylistWithTracks {
            name: "invalid".into(),
            track_ids: vec![1, 999],
        })
        .is_err()
    );
    core.publish();
    assert_eq!(
        core.store.playlist_summary_page(None, 0, 1).unwrap().total,
        0
    );
    assert_eq!(core.state.library.playlist_total, 0);
    assert_eq!(core.state.query_revisions(), before);
    core.command(Command::CreatePlaylistWithTracks {
        name: "valid".into(),
        track_ids: vec![2, 1, 2],
    })
    .unwrap();
    let playlist = core.store.playlist_summary_page(None, 0, 1).unwrap().rows[0].id;
    assert_eq!(core.store.playlist_track_ids(playlist, 10).unwrap(), [2, 1]);
    assert_eq!(core.state.library.playlist_total, 1);
}

#[test]
fn track_removal_validates_the_set_before_stopping_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let generation = core.generation;
    assert!(
        core.command(Command::RemoveTracks {
            track_ids: vec![1, 1, 999],
        })
        .is_err()
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.generation, generation);
    assert!(core.store.track(1).unwrap().is_some());
    core.command(Command::RemoveTracks {
        track_ids: vec![1, 1],
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.store.track(1).unwrap().is_none());
    assert!(
        core.store
            .queue_page(None, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .all(|entry| entry.track_id != 1)
    );
    assert_queue_saved(&core);
}

#[test]
fn failed_track_deletion_retains_persisted_current_occurrence_and_fences_audio() {
    for missing in [false, true] {
        for status in [
            PlaybackStatus::Stopped,
            PlaybackStatus::Paused,
            PlaybackStatus::Playing,
        ] {
            let (directory, mut core) = fixture_with_database(Some(10.0), true);
            if missing {
                let path = core.track(2).unwrap().path;
                let impacts = core
                    .store
                    .apply_scan(&ScanResult {
                        roots: vec![path],
                        ..ScanResult::default()
                    })
                    .unwrap();
                core.apply_scan_impacts(impacts).unwrap();
            }
            playing(&mut core, 3);
            match status {
                PlaybackStatus::Stopped => core.stop().unwrap(),
                PlaybackStatus::Paused => core.command(Command::Pause).unwrap(),
                PlaybackStatus::Playing => {}
            }
            core.save_queue().unwrap();
            let playlist = core
                .store
                .create_playlist_with_tracks("Keep", &[1, 2])
                .unwrap();
            core.playlist_created(playlist, true).unwrap();
            let connection =
                rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_track_delete BEFORE DELETE ON tracks
                 BEGIN SELECT RAISE(ABORT,'rejected track deletion'); END;",
                )
                .unwrap();
            let current = Arc::clone(core.state.current_track.as_ref().unwrap());
            let generation = core.generation;
            let late_started = started_event(&core, Some(10.0));
            let command = if missing {
                Command::RemoveMissingTracks
            } else {
                Command::RemoveTracks {
                    track_ids: vec![2, 2],
                }
            };
            let error = core.command(command).unwrap_err();
            assert!(format!("{error:#}").contains("rejected track deletion"));
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            assert_ne!(core.generation, generation);
            assert!(core.playback.is_none());
            assert_eq!(core.state.queue.current_id, Some(3));
            assert!(Arc::ptr_eq(
                core.state.current_track.as_ref().unwrap(),
                &current
            ));
            assert_eq!(
                queue_entries(&core),
                [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)]
            );
            assert_eq!(core.store.playlist_track_ids(playlist, 10).unwrap(), [1, 2]);
            assert_queue_saved(&core);
            core.audio_event(late_started).unwrap();
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
            assert_eq!(core.state.queue.current_id, Some(3));
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            core.load_initial_state().unwrap();
            assert_eq!(core.state.queue.current_id, Some(3));
            assert_eq!(core.state.current_track().unwrap().id, 2);
            connection
                .execute_batch("DROP TRIGGER reject_track_delete")
                .unwrap();
            let command = if missing {
                Command::RemoveMissingTracks
            } else {
                Command::RemoveTracks {
                    track_ids: vec![2, 2],
                }
            };
            core.command(command).unwrap();
            assert!(core.state.current_track.is_none());
            assert_eq!(core.state.queue.current_id, None);
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            assert_eq!(queue_entries(&core), [(1, 1), (4, 3), (5, 4)]);
            assert_queue_saved(&core);
        }
    }
}

#[test]
fn unrelated_track_deletion_preserves_current_occurrence_and_playback_status() {
    for missing in [false, true] {
        for status in [
            PlaybackStatus::Stopped,
            PlaybackStatus::Paused,
            PlaybackStatus::Playing,
        ] {
            let (_directory, mut core) = fixture();
            if missing {
                let path = core.track(1).unwrap().path;
                let impacts = core
                    .store
                    .apply_scan(&ScanResult {
                        roots: vec![path],
                        ..ScanResult::default()
                    })
                    .unwrap();
                core.apply_scan_impacts(impacts).unwrap();
            }
            playing(&mut core, 3);
            match status {
                PlaybackStatus::Stopped => core.stop().unwrap(),
                PlaybackStatus::Paused => core.command(Command::Pause).unwrap(),
                PlaybackStatus::Playing => {}
            }
            let generation = core.generation;
            let before = core.state.query_revisions();
            let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
            let command = if missing {
                Command::RemoveMissingTracks
            } else {
                Command::RemoveTracks { track_ids: vec![1] }
            };
            core.command(command).unwrap();
            assert_eq!(core.state.query_revisions().playlist, before.playlist);
            assert!(Arc::ptr_eq(
                &core.state.library.playlist_entry_revisions,
                &scopes
            ));
            assert_eq!(core.state.playback.status, status);
            assert_eq!(core.generation, generation);
            assert_eq!(core.state.queue.current_id, Some(3));
            assert_eq!(core.state.queue.current_position, Some(1));
            assert_eq!(core.state.current_track().unwrap().id, 2);
            assert_queue_saved(&core);
        }
    }
}

#[test]
fn interval_queue_moves_match_stable_selection_order_and_preserve_current() {
    let (_directory, mut core) = fixture();
    let original = core.queue_ids.clone();
    let current = original[2];
    core.store.set_queue_current(Some(current)).unwrap();
    for mask in 0..(1usize << original.len()) {
        for target in (0..=original.len()).chain(std::iter::once(usize::MAX)) {
            core.store.reorder_queue(&original).unwrap();
            core.queue_ids.clone_from(&original);
            core.refresh_queue_tracks().unwrap();
            let selected: Vec<_> = original
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, &id)| id)
                .collect();
            let mut expected: Vec<_> = original
                .iter()
                .copied()
                .filter(|id| !selected.contains(id))
                .collect();
            let insertion = target.min(expected.len());
            expected.splice(insertion..insertion, selected.iter().copied());
            let mut requested: Vec<_> = selected.iter().rev().copied().collect();
            if let Some(&id) = selected.first() {
                requested.push(id);
            }
            let revision = core.state.queue.revision;
            core.command(Command::MoveQueueEntries {
                queue_ids: requested,
                index: target,
            })
            .unwrap();
            assert_eq!(core.queue_ids, expected, "mask={mask} target={target}");
            assert_queue_saved(&core);
            assert_eq!(
                core.state.queue.revision,
                revision + u64::from(expected != original),
                "mask={mask} target={target}"
            );
            assert_eq!(core.state.queue.current_id, Some(current));
            assert_eq!(
                core.state.queue.current_position,
                expected.iter().position(|&id| id == current)
            );
            let ViewResponse::QueuePage(page) = core
                .view(&Query::QueuePage {
                    query: None,
                    offset: 0,
                    limit: MAX_QUERY_ROWS,
                })
                .unwrap()
            else {
                panic!("expected queue page")
            };
            assert_eq!(page.total, original.len());
            assert_eq!(
                page.rows
                    .iter()
                    .map(|row| (row.id, row.position))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .enumerate()
                    .map(|(position, &id)| (id, position))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn failed_queue_moves_preserve_memory_order_current_history_and_revision() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    playing(&mut core, 3);
    core.played = vec![1, 3, 5];
    core.played_cursor = 2;
    let order = core.queue_ids.clone();
    let state = core.state.queue.clone();
    let generation = core.generation;
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_queue_move AFTER UPDATE OF revision ON queue_meta
             BEGIN SELECT RAISE(ABORT,'queue move rejected'); END;",
        )
        .unwrap();
    for command in [
        Command::MoveQueue {
            queue_id: 3,
            index: 0,
        },
        Command::MoveQueueEntries {
            queue_ids: vec![5, 1, 3, 1],
            index: 1,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.queue_ids, order);
        assert_eq!(core.state.queue, state);
        assert_eq!(core.store.queue_state().unwrap(), state);
        assert_eq!(core.generation, generation);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        assert_eq!(core.played, [1, 3, 5]);
        assert_eq!(core.played_cursor, 2);
        assert_queue_saved(&core);
    }
}

#[test]
fn bulk_queue_edits_preserve_current_order_history_and_reject_invalid_ids_atomically() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 3);
    progress(&mut core, 1.5, 1.5);
    core.played = vec![1, 3, 5];
    core.played_cursor = 2;
    let generation = core.generation;
    let before = queue_entries(&core);
    for command in [
        Command::MoveQueueEntries {
            queue_ids: vec![3, 99],
            index: 0,
        },
        Command::RemoveQueueEntries {
            queue_ids: vec![3, 99],
        },
        Command::MoveQueue {
            queue_id: 3,
            index: usize::MAX,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(queue_entries(&core), before);
        assert_eq!(core.generation, generation);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    }
    core.command(Command::MoveQueueEntries {
        queue_ids: vec![5, 1, 3, 1],
        index: 1,
    })
    .unwrap();
    assert_eq!(
        queue_entries(&core),
        [(2, 2), (1, 1), (3, 2), (5, 4), (4, 3)]
    );
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(2));
    assert_eq!(core.generation, generation);
    assert_eq!(core.played, [1, 3, 5]);
    core.command(Command::MoveQueueEntries {
        queue_ids: vec![1, 3, 5],
        index: usize::MAX,
    })
    .unwrap();
    assert_eq!(
        queue_entries(&core),
        [(2, 2), (4, 3), (1, 1), (3, 2), (5, 4)]
    );
    core.command(Command::RemoveQueueEntries {
        queue_ids: vec![1, 1],
    })
    .unwrap();
    assert_eq!(core.generation, generation);
    assert_eq!(core.played, [3, 5]);
    assert_eq!(core.played_cursor, 1);
    core.command(Command::RemoveQueueEntries {
        queue_ids: vec![3, 3],
    })
    .unwrap();
    assert_eq!(core.state.queue.current_id, None);
    assert!(core.state.current_track().is_none());
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_queue_saved(&core);
}

#[test]
fn dedup_preserves_playing_duplicate_identity_at_first_position_and_prunes_history() {
    let (_directory, mut core) = fixture();
    core.command(Command::MoveQueue {
        queue_id: 3,
        index: 4,
    })
    .unwrap();
    playing(&mut core, 3);
    core.played = vec![1, 2, 4, 3, 2, 5];
    core.played_cursor = 4;
    let generation = core.generation;
    core.command(Command::DeduplicateQueue).unwrap();
    assert_eq!(queue_entries(&core), [(1, 1), (3, 2), (4, 3), (5, 4)]);
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(1));
    assert_eq!(core.played, [1, 4, 3, 5]);
    assert_eq!(core.played_cursor, 3);
    assert_eq!(core.generation, generation);
    assert_queue_saved(&core);
}

#[test]
fn randomize_changes_order_without_changing_current_or_history() {
    use rand::SeedableRng;
    let (_directory, mut core) = fixture();
    playing(&mut core, 3);
    core.played = vec![1, 3, 5];
    core.played_cursor = 2;
    let generation = core.generation;
    let mut expected = core.queue_ids.clone();
    core.randomize_queue(&mut rand::rngs::StdRng::seed_from_u64(42))
        .unwrap();
    let mut actual = core.queue_ids.clone();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.played, [1, 3, 5]);
    assert_eq!(core.played_cursor, 2);
    assert_eq!(core.generation, generation);
}

#[test]
fn empty_and_singleton_queue_edits_are_valid_and_empty_bulk_requests_are_no_ops() {
    let (_directory, mut core) = fixture();
    let before = core.state.queue.clone();
    core.command(Command::RemoveQueueEntries { queue_ids: vec![] })
        .unwrap();
    core.command(Command::MoveQueueEntries {
        queue_ids: vec![],
        index: usize::MAX,
    })
    .unwrap();
    assert_eq!(core.state.queue, before);
    for singleton in [false, true] {
        core.command(Command::ClearQueue).unwrap();
        if singleton {
            core.enqueue(&[2]).unwrap();
        }
        let before = core.queue_ids.clone();
        core.command(Command::RandomizeQueue).unwrap();
        core.command(Command::DeduplicateQueue).unwrap();
        assert_eq!(core.queue_ids, before);
    }
}

#[test]
fn removing_tracks_prunes_history_and_previous_retains_the_remaining_gap() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 4);
    core.state.playback.shuffle = true;
    core.played = vec![1, 2, 3, 4, 5];
    core.played_cursor = 4;
    core.command(Command::RemoveTracks { track_ids: vec![2] })
        .unwrap();
    assert_eq!(core.played, [1, 4, 5]);
    assert_eq!(core.played_cursor, 2);
    missing(&mut core, Command::Previous, 1);
    assert_eq!(core.played_cursor, 2);
    assert_queue_saved(&core);
}

#[test]
fn repeat_one_only_repeats_natural_end_and_repeat_all_revisits_singleton() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.playback.repeat = RepeatMode::One;
    missing(&mut core, Command::Next, 2);
    let generation = core.generation;
    let error = core
        .audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("1.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    core.command(Command::RemoveTracks {
        track_ids: vec![2, 3, 4],
    })
    .unwrap();
    core.state.playback.shuffle = true;
    core.state.playback.repeat = RepeatMode::All;
    core.played = vec![1];
    core.played_cursor = 1;
    missing(&mut core, Command::Next, 1);
}

#[test]
fn duplicate_queue_entries_count_as_separate_plays_but_share_recent_history() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 2);
    progress(&mut core, 3.0, 3.0);
    assert_eq!(play_count(&core, 2), 1);
    core.stop().unwrap();
    playing(&mut core, 3);
    progress(&mut core, 3.0, 3.0);
    progress(&mut core, 8.0, 8.0);
    assert_eq!(play_count(&core, 2), 2);
    let history = core.store.history(200).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].track_id, 2);
    assert_eq!(
        core.store.queue_track_id(2).unwrap(),
        core.store.queue_track_id(3).unwrap()
    );
    assert_ne!(core.queue_ids[1], core.queue_ids[2]);
}

#[test]
fn failed_playlist_start_does_not_keep_an_obsolete_queue_selection() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let playlist_id = core.store.create_playlist("missing").unwrap();
    core.store.add_playlist(playlist_id, &[2]).unwrap();
    core.playlist_created(playlist_id, true).unwrap();
    let error = core
        .command(Command::PlayPlaylist { playlist_id })
        .unwrap_err();
    assert!(error.to_string().contains("2.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.queue.current_id, None);
    missing(&mut core, Command::Resume, 2);
}
#[test]
fn shutdown_cancels_and_disconnects_a_backpressured_scan_worker() {
    let (_directory, mut core) = fixture();
    let cancellation = library::ScanCancellation::default();
    let observed = cancellation.clone();
    let sender = core.scan_tx.clone();
    let progress = ScanProgress {
        phase: ScanPhase::Discovering,
        processed: 0,
        total: None,
        errors: 0,
        path: None,
    };
    sender
        .send(ScanEvent::Progress {
            id: 1,
            progress: progress.clone(),
        })
        .unwrap();
    let (result_tx, result_rx) = bounded(1);
    core.active_scan = Some(ScanTask {
        status: ScanTaskStatus {
            id: 1,
            progress: progress.clone(),
            message: String::new(),
            error: None,
        },
        cancellation,
        worker: thread::spawn(move || {
            let disconnected = sender
                .send(ScanEvent::Finished(ScanFinished {
                    id: 1,
                    result: Ok(ScanResult::default()),
                    import: None,
                    progress,
                }))
                .is_err();
            result_tx
                .send((disconnected, observed.is_cancelled()))
                .unwrap();
        }),
    });
    core.shutdown_scan();
    assert_eq!(result_rx.recv().unwrap(), (true, true));
    assert!(core.active_scan.is_none());
    assert_eq!(
        core.scan_results.back().unwrap().progress.phase,
        ScanPhase::Cancelled
    );
}

#[test]
fn random_draws_include_current_weight_duplicates_and_ignore_forward_history() {
    use rand::SeedableRng;
    let (_directory, mut core) = fixture();
    core.state.queue.current_id = Some(3);
    core.played = vec![1, 3, 5];
    core.played_cursor = 1;
    let mut rng = rand::rngs::StdRng::seed_from_u64(9);
    let mut counts = [0usize; 5];
    let mut immediate_repeat = false;
    let mut previous = 3;
    for _ in 0..30_000 {
        let id = core.random_queue_id(&mut rng).unwrap();
        immediate_repeat |= previous == id;
        counts[id as usize - 1] += 1;
        previous = id;
    }
    assert!(immediate_repeat);
    assert!(counts.iter().all(|count| (5_500..6_500).contains(count)));
    assert!(counts[1] + counts[2] > counts[0] * 18 / 10);
    core.command(Command::ClearQueue).unwrap();
    core.enqueue(&[1]).unwrap();
    let id = core.queue_ids[0];
    assert_eq!(core.random_queue_id(&mut rng), Some(id));
    assert_eq!(core.random_queue_id(&mut rng), Some(id));
    for _ in 0..5000 {
        core.record_queue_history(id);
    }
    assert_eq!(core.played.len(), 4096);
    assert_eq!(core.played_cursor, 4096);
}
#[test]
fn queue_batch_revisions_ignore_current_only_changes_and_reject_metadata_edits() {
    let (_directory, mut core) = fixture();
    let expected = core.state.query_revisions();
    core.state.queue.current_id = Some(3);
    core.save_queue().unwrap();
    let query = Query::QueueBatch {
        query: None,
        after: None,
        limit: 2,
        expected: Some(expected),
    };
    assert!(matches!(core.view(&query), Ok(ViewResponse::QueueBatch(_))));
    core.command(Command::EditTrack {
        track_id: 2,
        title: Some("Changed".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert!(matches!(
        core.view(&query),
        Err(QueryError::RevisionChanged)
    ));
    let state = core.state.query_revisions();
    core.state.playback.position = 5.;
    assert_eq!(core.state.query_revisions().queue, state.queue);
}

#[test]
fn filtered_queue_continuations_bind_rank_query_and_revision_without_expected() {
    let (_directory, mut core) = fixture();
    for track_id in [1, 2, 4] {
        core.command(Command::EditTrack {
            track_id,
            title: Some("needle".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
    }
    let batch = |filter: Option<&str>, after| Query::QueueBatch {
        query: filter.map(str::to_owned),
        after,
        limit: 1,
        expected: None,
    };
    let mut after = None;
    let mut first = None;
    let mut rows = Vec::new();
    loop {
        let ViewResponse::QueueBatch(page) = core.view(&batch(Some("needle"), after)).unwrap()
        else {
            panic!("expected queue batch")
        };
        assert_eq!(page.rows[0].offset, rows.len());
        after = page.next;
        if first.is_none() {
            first = after;
        }
        rows.extend(page.rows);
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, rows.len());
    }
    assert_eq!(
        rows.iter().map(|row| row.position).collect::<Vec<_>>(),
        [0, 1, 2, 4]
    );
    assert_eq!(rows[1].track_id, rows[2].track_id);
    assert_ne!(rows[1].id, rows[2].id);
    let ViewResponse::QueuePage(jump) = core
        .view(&Query::QueuePage {
            query: Some("needle".into()),
            offset: 2,
            limit: 1,
        })
        .unwrap()
    else {
        panic!("expected queue page")
    };
    assert_eq!(jump.total, 4);
    assert_eq!(jump.rows, rows[2..3]);
    for filter in [Some("track"), None] {
        assert_eq!(
            core.view(&batch(filter, first)).unwrap_err(),
            QueryError::RevisionChanged
        );
    }
    // Playback selection alone preserves the matching continuation.
    core.state.queue.current_id = Some(rows[2].id);
    core.save_queue().unwrap();
    assert!(core.view(&batch(Some("NEEDLE"), first)).is_ok());
    let mut overflow = first.unwrap();
    overflow.offset = usize::MAX;
    assert!(matches!(
        core.view(&batch(Some("needle"), Some(overflow))),
        Err(QueryError::Message(_))
    ));
    core.enqueue(&[2]).unwrap();
    assert_eq!(
        core.view(&batch(Some("needle"), first)).unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::QueueBatch(fresh) = core.view(&batch(Some("needle"), None)).unwrap() else {
        panic!("expected queue batch")
    };
    core.command(Command::EditTrack {
        track_id: 2,
        title: Some("not matched".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert_eq!(
        core.view(&batch(Some("needle"), fresh.next)).unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::QueueBatch(restarted) = core.view(&batch(Some("needle"), None)).unwrap()
    else {
        panic!("expected queue batch")
    };
    assert_eq!(restarted.rows[0].offset, 0);
}

#[test]
fn filtered_playlist_continuations_reject_foreign_query_metadata_and_order() {
    let (_directory, mut core) = fixture();
    for track_id in [1, 3, 4] {
        core.command(Command::EditTrack {
            track_id,
            title: Some("needle".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
    }
    for name in ["first", "second"] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids: vec![1, 2, 3, 4],
        })
        .unwrap();
    }
    let batch = |playlist_id, filter: Option<&str>, after| Query::PlaylistEntryBatch {
        playlist_id,
        query: filter.map(str::to_owned),
        after,
        limit: 1,
        expected: None,
    };
    let mut after = None;
    let mut first = None;
    let mut rows = Vec::new();
    loop {
        let ViewResponse::PlaylistEntryBatch(page) =
            core.view(&batch(1, Some("needle"), after)).unwrap()
        else {
            panic!("expected playlist batch")
        };
        assert_eq!(page.rows[0].offset, rows.len());
        after = page.next;
        if first.is_none() {
            first = after;
        }
        rows.extend(page.rows);
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, rows.len());
    }
    assert_eq!(
        rows.iter().map(|row| row.position).collect::<Vec<_>>(),
        [0, 2, 3]
    );
    for query in [
        batch(2, Some("needle"), first),
        batch(1, Some("track"), first),
        batch(1, None, first),
    ] {
        assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
    }
    let other = core
        .store
        .playlist_entries_page(2, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::MovePlaylistEntry {
        entry_id: other,
        index: 3,
    })
    .unwrap();
    assert!(core.view(&batch(1, Some("needle"), first)).is_ok());
    core.command(Command::MovePlaylistEntry {
        entry_id: rows[0].id,
        index: 3,
    })
    .unwrap();
    assert_eq!(
        core.view(&batch(1, Some("needle"), first)).unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::PlaylistEntryBatch(fresh) =
        core.view(&batch(1, Some("needle"), None)).unwrap()
    else {
        panic!("expected playlist batch")
    };
    assert_eq!(
        (
            fresh.rows[0].track_id,
            fresh.rows[0].position,
            fresh.rows[0].offset
        ),
        (3, 1, 0)
    );
    core.command(Command::EditTrack {
        track_id: 3,
        title: Some("not matched".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert_eq!(
        core.view(&batch(1, Some("needle"), fresh.next))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::PlaylistEntryBatch(restarted) =
        core.view(&batch(1, Some("needle"), None)).unwrap()
    else {
        panic!("expected playlist batch")
    };
    assert_eq!(
        (
            restarted.rows[0].track_id,
            restarted.rows[0].position,
            restarted.rows[0].offset
        ),
        (4, 2, 0)
    );
}

#[test]
fn configure_failed_save_keeps_desired_settings_and_does_not_deliver_audio() {
    let (_directory, mut core) = fixture();
    let audio = recording_audio(&mut core);
    let previous = core.state.system.config.clone();
    std::fs::create_dir(&core.state.system.config_path).unwrap();
    let mut config = previous.as_ref().clone();
    config.volume = 0.25;
    config.shuffle = true;
    config.repeat = RepeatMode::All;
    config.still_mode = true;
    config.ffmpeg_enabled = false;
    config.page_size = 32;
    config.output_device = Some("configured-output".into());
    Arc::make_mut(&mut core.state.system.devices).push("configured-output".into());
    core.played = vec![1, 2];
    core.played_cursor = 2;
    let playback = core.state.playback.clone();
    let device = core.state.system.selected_device.clone();
    let ffmpeg_status = core.state.system.ffmpeg_status.clone();

    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap_err();

    assert_eq!(*core.state.system.config, *previous);
    assert_eq!(core.state.playback.volume, playback.volume);
    assert_eq!(core.state.playback.shuffle, playback.shuffle);
    assert_eq!(core.state.playback.repeat, playback.repeat);
    assert_eq!(core.state.system.selected_device, device);
    assert_eq!(core.state.system.ffmpeg_status, ffmpeg_status);
    assert_eq!(core.played, [1, 2]);
    assert_eq!(core.played_cursor, 2);
    assert!(matches!(
        audio.recv_timeout(Duration::from_millis(50)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
}

#[test]
fn configure_rejected_preparation_keeps_saved_and_desired_settings() {
    let (_directory, mut core) = fixture();
    let audio = recording_audio(&mut core);
    let previous = core.state.system.config.clone();
    previous.save(&core.state.system.config_path).unwrap();
    let mut config = previous.as_ref().clone();
    config.volume = 0.25;
    config.still_mode = true;
    config.page_size = 0;

    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap_err();

    assert_eq!(*core.state.system.config, *previous);
    assert_eq!(core.state.playback.volume, previous.volume);
    assert_eq!(
        Config::load(&core.state.system.config_path).unwrap(),
        *previous
    );
    assert!(matches!(
        audio.recv_timeout(Duration::from_millis(50)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
}

#[test]
fn output_settings_errors_follow_current_request_after_stop() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    core.command(Command::Stop).unwrap();
    core.command(Command::Device {
        name: Some("PipeWire: desired".into()),
    })
    .unwrap();
    let auto_mix = core.state.system.config.pipewire_auto_mix;
    let request = core.output_settings_request;
    core.command(Command::Device {
        name: Some("PipeWire: other".into()),
    })
    .unwrap();
    core.command(Command::Device {
        name: Some("PipeWire: desired".into()),
    })
    .unwrap();
    core.audio_event(AudioEvent::OutputSettingsFailed {
        request,
        device: Some("PipeWire: desired".into()),
        auto_mix,
        message: "superseded same-target failure".into(),
    })
    .unwrap();
    assert!(core.state.system.last_error.is_none());
    for (device, mix) in [
        ("PipeWire: superseded", auto_mix),
        ("PipeWire: desired", !auto_mix),
    ] {
        core.audio_event(AudioEvent::OutputSettingsFailed {
            request: core.output_settings_request,
            device: Some(device.into()),
            auto_mix: mix,
            message: "obsolete failure".into(),
        })
        .unwrap();
        assert!(core.state.system.last_error.is_none());
    }
    core.audio_event(AudioEvent::OutputSettingsFailed {
        request: core.output_settings_request,
        device: Some("PipeWire: desired".into()),
        auto_mix,
        message: "desired output unavailable".into(),
    })
    .unwrap();
    assert_eq!(
        core.state.system.last_error.as_deref(),
        Some("desired output unavailable")
    );
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("PipeWire: desired")
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);

    core.command(Command::Stop).unwrap();
    let queue_id = core.queue_ids[0];
    playing(&mut core, queue_id);
    let playback_generation = core.generation;
    core.state.system.last_error = None;
    core.audio_event(AudioEvent::OutputSettingsFailed {
        request: core.output_settings_request,
        device: Some("PipeWire: desired".into()),
        auto_mix,
        message: "configuration failed before the newer playback".into(),
    })
    .unwrap();
    assert_eq!(
        core.state.system.last_error.as_deref(),
        Some("configuration failed before the newer playback")
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.generation, playback_generation);
}

#[test]
fn unavailable_output_targets_remain_desired_and_persist_across_entry_points() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    Arc::make_mut(&mut core.state.system.devices).clear();
    let mut config = core.state.system.config.as_ref().clone();
    config.ffmpeg_enabled = false;
    config.output_device = Some("PipeWire: unavailable-configured-sink".into());
    core.command(Command::Configure {
        config: Arc::new(config),
    })
    .unwrap();
    assert_eq!(
        Config::load(&core.state.system.config_path)
            .unwrap()
            .output_device,
        core.state.system.selected_device
    );
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("PipeWire: unavailable-configured-sink")
    );
    core.command(Command::Device {
        name: Some("PipeWire: unavailable-replacement-sink".into()),
    })
    .unwrap();
    core.flush_config().unwrap();
    assert_eq!(
        Config::load(&core.state.system.config_path)
            .unwrap()
            .output_device
            .as_deref(),
        Some("PipeWire: unavailable-replacement-sink")
    );
}

#[test]
fn configure_saved_but_disconnected_audio_retains_desired_settings() {
    let (_directory, mut core) = fixture();
    let (commands, receiver) = bounded(64);
    drop(receiver);
    core.engine.replace_commands(commands);
    let mut config = core.state.system.config.as_ref().clone();
    config.volume = 0.25;
    config.shuffle = true;
    config.repeat = RepeatMode::All;
    config.still_mode = true;
    config.ffmpeg_enabled = false;
    config.page_size = 32;
    config.output_device = Some("configured-output".into());
    Arc::make_mut(&mut core.state.system.devices).push("configured-output".into());
    core.played = vec![1, 2];
    core.played_cursor = 2;
    let expected = config.clone();

    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap_err();

    assert_eq!(*core.state.system.config, expected);
    assert_eq!(core.state.playback.volume, expected.volume);
    assert_eq!(core.state.playback.shuffle, expected.shuffle);
    assert_eq!(core.state.playback.repeat, expected.repeat);
    assert_eq!(core.state.system.selected_device, expected.output_device);
    assert_eq!(core.state.system.ffmpeg_status, "disabled");
    assert!(core.played.is_empty());
    assert_eq!(core.played_cursor, 0);
    assert_eq!(
        Config::load(&core.state.system.config_path).unwrap(),
        expected
    );
    assert!(!core.flush_config().unwrap());
    core.publish();
    assert_eq!(*core.shared.read().system.config, expected);
}

#[test]
fn still_mode_toggle_commits_without_accessing_unwritable_config_path() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    let previous = core.state.system.config.still_mode;
    std::fs::create_dir(&core.state.system.config_path).unwrap();
    core.update_gui_config(GuiConfigChange::ToggleStillMode)
        .unwrap();
    assert_eq!(core.state.system.config.still_mode, !previous);
    assert!(core.state.system.config_path.is_dir());
}

#[test]
fn still_mode_applied_but_disconnected_audio_retains_unsaved_settings() {
    let (_directory, mut core) = fixture();
    let (commands, receiver) = bounded(1);
    drop(receiver);
    core.engine.replace_commands(commands);
    // Pending playback settings must be incorporated in the same memory snapshot.
    core.state.playback.volume = 0.25;
    core.state.playback.shuffle = true;
    core.state.playback.repeat = RepeatMode::All;
    core.state.system.selected_device = Some("pending-output".into());
    let mut expected = core.state.system.config.as_ref().clone();
    expected.volume = 0.25;
    expected.shuffle = true;
    expected.repeat = RepeatMode::All;
    expected.output_device = Some("pending-output".into());

    for enabled in [true, false] {
        expected.still_mode = enabled;
        core.update_gui_config(GuiConfigChange::ToggleStillMode)
            .unwrap_err();

        assert_eq!(*core.state.system.config, expected);
        assert_eq!(core.state.playback.volume, expected.volume);
        assert_eq!(core.state.playback.shuffle, expected.shuffle);
        assert_eq!(core.state.playback.repeat, expected.repeat);
        assert_eq!(core.state.system.selected_device, expected.output_device);
        assert!(!core.state.system.config_path.exists());
        core.publish();
        assert_eq!(*core.shared.read().system.config, expected);
    }
}

#[test]
fn metadata_edit_refreshes_published_history_without_rewriting_old_snapshots() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    playing(&mut core, 2);
    core.publish();
    let before = core.shared.read().library.history.clone();
    let before_ids = before.iter().map(|row| row.track_id).collect::<Vec<_>>();
    let before_played_at = before
        .iter()
        .find(|row| row.track_id == 1)
        .unwrap()
        .played_at;
    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("Renamed history".to_owned()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    core.publish();
    let published = core.shared.read().library.history.clone();
    let row = published.iter().find(|row| row.track_id == 1).unwrap();
    assert_eq!(row.title, "Renamed history");
    assert_eq!(row.played_at, before_played_at);
    assert_eq!(
        published.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        before_ids
    );
    assert_eq!(
        published
            .iter()
            .find(|row| row.track_id == 2)
            .unwrap()
            .title,
        "Track 2"
    );
    assert_eq!(
        before.iter().find(|row| row.track_id == 1).unwrap().title,
        "Track 1"
    );
}

#[test]
fn crossing_play_threshold_refreshes_published_history_count_during_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.publish();
    let before = core.shared.read().library.history.clone();
    progress(&mut core, 2.0, 2.0);
    core.publish();
    assert_eq!(core.shared.read().library.history[0].play_count, 0);
    progress(&mut core, 3.0, 3.0);
    core.publish();
    let published = core.shared.read().library.history.clone();
    assert_eq!(published[0].play_count, 1);
    assert_eq!(published[0].track_id, 1);
    assert_eq!(published[0].played_at, before[0].played_at);
    assert_eq!(before[0].play_count, 0);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
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
        assert!(status.error.is_some());
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
        if matches!(&event, ScanEvent::KnownSource { .. }) {
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

#[test]
fn ffmpeg_external_load_result_survives_runtime_disable_and_reenable() {
    let (directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    Arc::make_mut(&mut core.state.system.config).ffmpeg_enabled = false;
    core.state.system.ffmpeg_status = "disabled".into();
    let (handle, control, worker, _publications) = navigation_runtime(core);
    let changes = handle.subscribe();
    let configure = |enabled| {
        let mut config = handle.config_snapshot().as_ref().clone();
        config.ffmpeg_enabled = enabled;
        let ack = handle.operation_ack(
            Operation::Command(Command::Configure {
                config: Arc::new(config),
            }),
            Duration::from_secs(5),
        );
        assert!(ack.ok, "{:?}", ack.error);
    };

    configure(true);
    // An enhanced probe attempts the same process-wide load as GUI waveform
    // work, without an audio-engine event. A missing file avoids decoder I/O;
    // both successful and failed library loads must become terminal status.
    assert!(audio::probe_details(&directory.path().join("missing.wav"), true, true).is_err());
    let expected = audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"));
    assert_ne!(expected, "not loaded");
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.state.read().system.ffmpeg_status != expected {
        changes
            .recv_deadline(deadline)
            .expect("external decoder load result must reach core state");
    }

    configure(false);
    assert_eq!(handle.state.read().system.ffmpeg_status, "disabled");
    configure(true);
    assert_eq!(handle.state.read().system.ffmpeg_status, expected);
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}
