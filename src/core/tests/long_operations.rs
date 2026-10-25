use super::*;

fn saving_with_write_lock() -> (tempfile::TempDir, Core, rusqlite::Connection) {
    saving_with_write_lock_and_processing(false)
}

fn saving_with_write_lock_and_processing(
    processing: bool,
) -> (tempfile::TempDir, Core, rusqlite::Connection) {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    write_pcm_frames(&directory.path().join("1.wav"), 100);
    if processing {
        core.command(Command::StartProcessing {
            track_id: 1,
            operation: ProcessingOperation::Denoise,
            profile: None,
        })
        .unwrap();
    }
    pending(&mut core, 1);
    core.command(Command::Scan {
        paths: vec![directory.path().join("1.wav")],
        force: true,
    })
    .unwrap();
    let finished = loop {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if let ScanEvent::Finished(finished) = event {
            break finished;
        }
        core.scan_event(event);
    };
    let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    core.scan_event(ScanEvent::Finished(finished));
    assert!(core.database_maintenance_active());
    (directory, core, lock)
}

fn finish_saving(core: &mut Core, lock: rusqlite::Connection) {
    lock.execute_batch("ROLLBACK").unwrap();
    while core.active_scan.is_some() {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        core.scan_event(event);
    }
    core.flush_deferred_playback().unwrap();
}

#[test]
fn processing_cancellation_remains_available_while_scan_holds_saving_phase() {
    let (_directory, mut core, lock) = saving_with_write_lock_and_processing(true);
    assert!(core.database_maintenance_active());
    core.command(Command::CancelProcessing { id: 1 }).unwrap();
    loop {
        let event = wait_on(&core.executor, async {
            tokio::time::timeout(Duration::from_secs(5), core.processing.receiver.recv())
                .await
                .unwrap()
                .unwrap()
        });
        let finished = matches!(event, crate::processing::Event::Finished { .. });
        core.processing_event(event);
        if finished {
            break;
        }
    }
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Cancelled
    );
    assert!(core.processing.results.is_empty());
    finish_saving(&mut core, lock);
}

#[test]
fn scan_saving_keeps_controls_responsive_and_preserves_retired_statistics() {
    let (_directory, mut core, lock) = saving_with_write_lock();
    let _audio = recording_audio(&mut core);
    started(&mut core, Some(10.0)).unwrap();
    progress(&mut core, 3.0, 3.0);
    core.command(Command::Volume { value: 0.33 }).unwrap();
    core.command(Command::Stop).unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.playback.volume, 0.33);
    assert_eq!(play_count(&core, 1), 0);
    assert!(core.store.track(1).unwrap().unwrap().last_played.is_none());
    assert!(
        core.command(Command::Enqueue { track_ids: vec![2] })
            .is_err()
    );
    let id = core.active_scan.as_ref().unwrap().status.id;
    assert!(core.command(Command::CancelScan { id }).is_err());
    finish_saving(&mut core, lock);
    assert_eq!(play_count(&core, 1), 1);
    assert!(core.store.track(1).unwrap().unwrap().last_played.is_some());
    assert_eq!(core.store.history(MAX_QUERY_ROWS).unwrap()[0].track_id, 1);
    core.flush_deferred_playback().unwrap();
    assert_eq!(play_count(&core, 1), 1);
    core.command(Command::Enqueue { track_ids: vec![2] })
        .unwrap();
}

#[test]
fn eof_during_scan_saving_preserves_later_pause_intent() {
    let (_directory, mut core, lock) = saving_with_write_lock();
    let _audio = recording_audio(&mut core);
    core.state.playback.repeat = RepeatMode::One;
    started(&mut core, Some(10.0)).unwrap();
    progress(&mut core, 3.0, 3.0);
    core.audio_event(AudioEvent::Ended {
        generation: core.generation,
        bitrate_bps: None,
    })
    .unwrap();
    core.command(Command::Pause).unwrap();
    assert_eq!(core.pending_navigation, Some(PlaybackStatus::Paused));
    finish_saving(&mut core, lock);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(play_count(&core, 1), 1);
}

#[test]
fn export_snapshot_keeps_order_and_metadata_across_concurrent_edits() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    core.command(Command::CreatePlaylistWithTracks {
        name: "Snapshot".into(),
        track_ids: vec![1, 2, 3],
    })
    .unwrap();
    let id = 1;
    let path = directory.path().join("library.db");
    let snapshot = Store::open_export(&path, id).unwrap();
    // Opening validates existence and pins metadata before the first batch is read.
    core.command(Command::EditTrack {
        track_id: 2,
        title: Some("New title".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    let entry_id = core
        .store
        .playlist_entries_page(id, None, 1, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::RemovePlaylistEntry { entry_id })
        .unwrap();
    let first = snapshot.playlist_export_batch(id, None, 1).unwrap();
    assert_eq!(first.rows[0].path, directory.path().join("1.wav"));
    let remaining = snapshot.playlist_export_batch(id, first.next, 3).unwrap();
    assert_eq!(
        remaining
            .rows
            .iter()
            .map(|track| track.path.clone())
            .collect::<Vec<_>>(),
        [
            directory.path().join("2.wav"),
            directory.path().join("3.wav")
        ]
    );
    assert_eq!(remaining.rows[0].title, "Track 2");
    let fresh = Store::open_export(&path, id).unwrap();
    let current = fresh.playlist_export_batch(id, None, 3).unwrap();
    assert_eq!(
        current
            .rows
            .iter()
            .map(|track| track.path.clone())
            .collect::<Vec<_>>(),
        [
            directory.path().join("1.wav"),
            directory.path().join("3.wav")
        ]
    );
}

#[test]
fn export_cancelled_after_last_row_preserves_existing_destination() {
    let (directory, core) = fixture();
    let destination = directory.path().join("playlist.m3u8");
    std::fs::write(&destination, b"original\n").unwrap();
    let cancellation = library::ScanCancellation::default();
    let final_check = cancellation.clone();
    let id = core
        .store
        .create_playlist_with_tracks("Cancel", &[1])
        .unwrap();
    let track = core
        .store
        .playlist_export_batch(id, None, 1)
        .unwrap()
        .rows
        .remove(0);
    let tracks = std::iter::once(Ok(track)).chain(std::iter::from_fn(move || {
        final_check.cancel();
        None
    }));
    assert!(
        wait_on(
            &core.executor,
            library::export_m3u(&destination, futures::stream::iter(tracks), &cancellation),
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"original\n");
}

#[test]
fn failed_deferred_activity_preserves_controls_and_guards_producer_replacement() {
    let (directory, mut core, lock) = saving_with_write_lock();
    let _audio = recording_audio(&mut core);
    write_pcm(&directory.path().join("2.wav"));
    started(&mut core, Some(10.0)).unwrap();
    progress(&mut core, 3.0, 3.0);
    core.command(Command::Stop).unwrap();
    lock.execute_batch("ROLLBACK").unwrap();
    while core.active_scan.is_some() {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        core.scan_event(event);
    }
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_activity BEFORE UPDATE OF last_played ON tracks BEGIN SELECT RAISE(FAIL,'activity unavailable'); END").unwrap();
    assert!(core.flush_deferred_playback().is_err());
    assert!(core.retired_playback.is_some());
    assert!(!core.playback_write_deferred);
    assert_eq!(play_count(&core, 1), 1);
    assert!(core.store.track(1).unwrap().unwrap().last_played.is_none());
    core.command(Command::Volume { value: 0.4 }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    core.state.system.last_error = Some("previous failure".into());
    core.command(Command::DismissError).unwrap();
    assert_eq!(core.state.playback.volume, 0.4);
    assert_eq!(core.state.playback.repeat, RepeatMode::All);
    assert!(core.state.system.last_error.is_none());
    assert!(core.retired_playback.is_some());
    assert!(core.command(Command::PlayQueue { queue_id: 2 }).is_err());
    assert_eq!(core.state.queue.current_id, Some(1));
    assert!(core.playback.is_none());
    assert!(
        core.command(Command::RemoveTracks { track_ids: vec![1] })
            .is_err()
    );
    assert!(core.store.track(1).unwrap().is_some());
    core.command(Command::Scan {
        paths: vec![directory.path().join("1.wav")],
        force: true,
    })
    .unwrap();
    while core.active_scan.is_some() {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        core.scan_event(event);
    }
    let failed_scan = core.scan_results.back().unwrap();
    assert_eq!(failed_scan.progress.phase, ScanPhase::Failed);
    assert!(!failed_scan.library_committed);
    assert!(core.retired_playback.is_some());
    connection
        .execute_batch("DROP TRIGGER reject_activity")
        .unwrap();
    core.command(Command::PlayQueue { queue_id: 2 }).unwrap();
    assert!(core.retired_playback.is_none());
    assert_eq!(play_count(&core, 1), 1);
    assert!(core.store.track(1).unwrap().unwrap().last_played.is_some());
}

#[test]
fn existing_playback_controls_remain_usable_while_database_is_write_locked() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    let _audio = recording_audio(&mut core);
    playing(&mut core, 1);
    let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::One,
    })
    .unwrap();
    core.command(Command::Toggle).unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    core.command(Command::Toggle).unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.state.playback.volume, 0.25);
    assert_eq!(core.state.playback.repeat, RepeatMode::One);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_queue_saved(&core);
    lock.execute_batch("ROLLBACK").unwrap();
}
