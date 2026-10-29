use super::*;

fn admission_fixture() -> (tempfile::TempDir, Core) {
    fixture_with_database(Some(10.0))
}

fn next_admission_event(core: &mut Core) -> crate::core::admission::Event {
    futures::executor::block_on(async {
        rivu_protocol::transport::timeout(
            Duration::from_secs(5),
            core.active_external_admission
                .as_mut()
                .unwrap()
                .next_event(),
        )
        .await
        .unwrap()
    })
}

fn finish_admission(core: &mut Core) {
    while core.active_external_admission.is_some() {
        let event = next_admission_event(core);
        core.external_admission_event(event);
    }
}

#[test]
fn external_admission_appends_valid_subset_without_roots_autoplay_or_playlist_changes() {
    let (directory, mut core) = admission_fixture();
    let first = directory.path().join("external-first.wav");
    let second = directory.path().join("external-second.wav");
    write_pcm_frames(&first, 1);
    write_pcm_frames(&second, 1);
    Arc::make_mut(&mut core.state.configuration.config)
        .library_roots
        .clear();
    Arc::make_mut(&mut core.state.configuration.config).scan_min_duration_secs = 60;
    let config = Arc::clone(&core.state.configuration.config);
    let before_queue = core.store.queue_ids().unwrap();
    let before_playback = core.state.playback.clone();
    let before_playlists = core.state.library.playlist_total;
    let before_playlist_revision = core.state.library.playlist_revision;
    let before_total = core.state.library.track_total;
    let directory_input = directory.path().join("directory.wav");
    std::fs::create_dir(&directory_input).unwrap();
    write_pcm_frames(&directory_input.join("hidden.wav"), 1);
    core.command(Command::QueueExternalPaths {
        paths: vec![
            second.clone(),
            directory_input,
            first.clone(),
            second.clone(),
            directory.path().join("missing.wav"),
        ],
        index: None,
        expected_revision: None,
    })
    .unwrap();
    let id = core.active_external_admission.as_ref().unwrap().status.id;
    assert!(core.state.system.scanning);
    assert_eq!(core.state.system.scan_task_id, Some(id));
    core.publish();
    let snapshot = crate::projection::GuiSnapshot::from_core(&core.shared.read());
    assert!(
        snapshot
            .system
            .scan_visible(crate::projection::MediaTaskOwner::Queue)
    );
    assert!(
        !snapshot
            .system
            .scan_visible(crate::projection::MediaTaskOwner::Library)
    );
    assert_eq!(core.store.queue_ids().unwrap(), before_queue);
    let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("expected media task")
    };
    assert_eq!(status.id, id);
    assert_eq!(status.progress.phase, ScanPhase::Discovering);
    finish_admission(&mut core);
    core.publish();
    let snapshot = crate::projection::GuiSnapshot::from_core(&core.shared.read());
    assert!(
        snapshot
            .system
            .scan_visible(crate::projection::MediaTaskOwner::Queue)
    );
    assert!(
        !snapshot
            .system
            .scan_visible(crate::projection::MediaTaskOwner::Library)
    );
    assert!(snapshot.system.last_error.is_some());
    let added = core
        .store
        .queue_page(None, before_queue.len(), 10)
        .unwrap()
        .rows;
    assert_eq!(added.len(), 3);
    let paths: Vec<_> = added
        .iter()
        .map(|row| core.store.track(row.track_id).unwrap().unwrap().path)
        .collect();
    assert_eq!(paths, vec![second.clone(), first.clone(), second]);
    assert_eq!(added[0].track_id, added[2].track_id);
    assert_ne!(added[0].id, added[2].id);
    assert_eq!(
        &core.store.queue_ids().unwrap()[..before_queue.len()],
        &before_queue
    );
    assert_eq!(core.state.playback, before_playback);
    assert_eq!(core.state.library.playlist_total, before_playlists);
    assert_eq!(
        core.state.library.playlist_revision,
        before_playlist_revision
    );
    assert_eq!(core.state.library.track_total, before_total + 2);
    assert!(Arc::ptr_eq(&core.state.configuration.config, &config));
    let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("expected retained media task")
    };
    assert!(status.library_committed);
    assert!(status.message.contains("skipped 2 invalid paths"));
    assert_eq!(status.progress.errors, 2);
    assert!(!core.state.system.scanning);
    assert_eq!(core.state.system.scan_task_id, Some(id));
    let revisions = core.state.revisions;
    core.publish();
    assert_eq!(core.state.revisions, revisions);
    core.command(Command::DismissScanResult).unwrap();
    core.publish();
    let snapshot = crate::projection::GuiSnapshot::from_core(&core.shared.read());
    assert!(
        !snapshot
            .system
            .scan_visible(crate::projection::MediaTaskOwner::Queue)
    );
    assert!(snapshot.system.scan_task_id.is_none());
}

#[test]
fn external_insertions_invalidate_queue_intents_even_for_unchanged_registered_sources() {
    for index in [None, Some(0)] {
        let (directory, mut core) = admission_fixture();
        let path = directory.path().join("external.wav");
        write_pcm_frames(&path, 1);
        for _ in 0..2 {
            let before = core.state.query_revisions();
            core.start_external_admission(vec![path.clone()], index, index.map(|_| before.queue))
                .unwrap();
            finish_admission(&mut core);
            assert_ne!(core.state.queue.revision, before.queue);
            let stale = Query::QueueBatch {
                query: None,
                after: None,
                limit: 2,
                expected: Some(before),
            };
            assert!(matches!(
                core.view(&stale),
                Err(QueryError::RevisionChanged)
            ));
            assert!(
                core.start_external_admission(vec![path.clone()], Some(0), Some(before.queue))
                    .is_err()
            );
        }
    }
}

#[test]
fn revision_changed_during_probe_rejects_entire_batch_without_registration_or_append() {
    let (directory, mut core) = admission_fixture();
    let path = directory.path().join("external.wav");
    write_pcm_frames(&path, 1);
    let before_total = core.state.library.track_total;
    let revision = core.state.queue.revision;
    core.start_external_admission(vec![path.clone()], Some(1), Some(revision))
        .unwrap();
    let id = core.active_external_admission.as_ref().unwrap().status.id;
    core.command(Command::Enqueue { track_ids: vec![1] })
        .unwrap();
    let queue = core.store.queue_ids().unwrap();
    finish_admission(&mut core);
    assert_eq!(core.store.queue_ids().unwrap(), queue);
    assert_eq!(core.state.library.track_total, before_total);
    assert_eq!(core.store.track_count().unwrap(), before_total);
    assert!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))
            .unwrap()
            .is_none()
    );
    let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("expected task")
    };
    assert!(!status.library_committed);
    assert_eq!(status.progress.phase, ScanPhase::Failed);
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("Queue revision changed")
    );
}

#[test]
fn cancel_scan_is_intent_until_preparation_joins_and_leaves_no_residue() {
    let (directory, mut core) = admission_fixture();
    let path = directory.path().join("external.wav");
    write_pcm_frames(&path, 1);
    let queue = core.store.queue_ids().unwrap();
    let total = core.state.library.track_total;
    core.start_external_admission(vec![path.clone()], None, None)
        .unwrap();
    let id = core.active_external_admission.as_ref().unwrap().status.id;
    let held_lookup = loop {
        let event = next_admission_event(&mut core);
        if matches!(event, crate::core::admission::Event::KnownSources { .. }) {
            break event;
        }
        core.external_admission_event(event);
    };
    core.command(Command::CancelScan { id }).unwrap();
    assert!(core.active_external_admission.is_some());
    assert!(core.state.system.scanning);
    assert_eq!(core.state.system.scan_task_id, Some(id));
    finish_admission(&mut core);
    drop(held_lookup);
    assert_eq!(core.store.queue_ids().unwrap(), queue);
    assert_eq!(core.store.track_count().unwrap(), total);
    assert!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))
            .unwrap()
            .is_none()
    );
    let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("expected task")
    };
    assert_eq!(status.progress.phase, ScanPhase::Cancelled);
    assert!(!status.library_committed);
    assert!(!core.state.system.scanning);
}

#[test]
fn admission_owns_media_slot_and_requires_versioned_positions() {
    let (directory, mut core) = admission_fixture();
    let path = directory.path().join("external.wav");
    write_pcm_frames(&path, 1);
    assert!(
        core.start_external_admission(Vec::new(), None, None)
            .is_err()
    );
    assert!(
        core.start_external_admission(vec![path.clone()], Some(0), None)
            .is_err()
    );
    core.start_external_admission(vec![path.clone()], None, None)
        .unwrap();
    assert!(
        core.start_external_admission(vec![path.clone()], None, None)
            .is_err()
    );
    assert!(
        core.command(Command::Scan {
            paths: vec![path.clone()],
            force: false
        })
        .is_err()
    );
    assert!(
        core.command(Command::CheckOtherTracks { force: false })
            .is_err()
    );
    assert!(core.command(Command::OptimizeDatabase).is_err());
    core.shutdown_external_admission();
    assert!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        core.scan_results.back().unwrap().progress.phase,
        ScanPhase::Cancelled
    );
}

#[test]
fn immediate_ack_exposes_task_id_before_background_saving_finishes() {
    let (directory, core) = admission_fixture();
    let path = directory.path().join("external.wav");
    write_pcm_frames(&path, 1);
    let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (handle, control, worker, _) = navigation_runtime(core);
    let ack = futures::executor::block_on(handle.request_ack(Command::QueueExternalPaths {
        paths: vec![path.clone()],
        index: None,
        expected_revision: None,
    }));
    assert_eq!(ack.status, AckStatus::Succeeded);
    let id = ack
        .scan_id
        .expect("admitted external files expose a media task identity");
    let ViewResponse::ScanTask(status) = handle.query(Query::ScanTask { id }).result.unwrap()
    else {
        panic!("expected active task")
    };
    assert_eq!(status.id, id);
    assert!(!status.library_committed);
    lock.execute_batch("ROLLBACK").unwrap();
    control.request_shutdown().unwrap();
    let core = worker.join().unwrap();
    // Shutdown may cancel preparation or join Saving, but the same retained task
    // must truthfully distinguish those outcomes; the admission Ack is not a commit.
    let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("expected retained task")
    };
    assert_eq!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))
            .unwrap()
            .is_some(),
        status.library_committed
    );
}

#[test]
fn saving_rejects_cancel_and_shutdown_joins_then_applies_committed_queue_facts() {
    let (directory, mut core) = admission_fixture();
    let path = directory.path().join("external.wav");
    write_pcm_frames(&path, 1);
    let total = core.state.library.track_total;
    let queue_total = core.state.queue.total;
    let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    core.start_external_admission(vec![path.clone(), path.clone()], None, None)
        .unwrap();
    let id = core.active_external_admission.as_ref().unwrap().status.id;
    while !core.active_external_admission.as_ref().unwrap().is_saving() {
        let event = next_admission_event(&mut core);
        core.external_admission_event(event);
    }
    assert!(core.database_maintenance_active());
    assert!(core.command(Command::CancelScan { id }).is_err());
    core.command(Command::Volume { value: 0.42 }).unwrap();
    assert_eq!(core.state.playback.volume, 0.42);
    assert!(
        core.store
            .track_id_for_identity(crate::store::TrackIdentity::File(&path))
            .unwrap()
            .is_none()
    );
    lock.execute_batch("ROLLBACK").unwrap();
    core.shutdown_external_admission();
    assert!(core.active_external_admission.is_none());
    assert!(!core.database_maintenance_active());
    assert_eq!(core.state.library.track_total, total + 1);
    assert_eq!(core.state.queue.total, queue_total + 2);
    assert_eq!(
        core.store.queue_ids().unwrap().len(),
        core.state.queue.total
    );
    let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("expected retained task")
    };
    assert!(status.library_committed);
    assert_eq!(status.progress.phase, ScanPhase::Completed);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
}

#[test]
fn committed_queue_and_task_facts_survive_current_track_projection_failure() {
    let (directory, mut core) = admission_fixture();
    let path = directory.path().join("1.wav");
    write_pcm_frames(&path, 1);
    core.store
        .set_queue_current(Some(core.store.queue_id_at(0).unwrap().unwrap()))
        .unwrap();
    core.refresh_queue_tracks().unwrap();
    let before = core.state.queue.total;
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER corrupt_admission_projection AFTER UPDATE OF raw_title ON tracks WHEN NEW.id=1 BEGIN UPDATE tracks SET channels='unreadable' WHERE id=1; END;").unwrap();
    core.start_external_admission(vec![path], None, None)
        .unwrap();
    let id = core.active_external_admission.as_ref().unwrap().status.id;
    finish_admission(&mut core);
    assert_eq!(core.state.queue.total, before + 1);
    assert_eq!(
        core.store.queue_ids().unwrap().len(),
        core.state.queue.total
    );
    let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
        panic!("expected task")
    };
    assert!(status.library_committed);
    assert_eq!(status.progress.phase, ScanPhase::Failed);
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("Refreshing current track")
    );
}

#[test]
fn external_media_waits_share_scan_identity_and_report_all_terminal_paths() {
    for expected in [
        ScanPhase::Completed,
        ScanPhase::Failed,
        ScanPhase::Cancelled,
    ] {
        for shutdown in [false, true] {
            if shutdown && expected != ScanPhase::Cancelled {
                continue;
            }
            let (directory, mut core) = admission_fixture();
            let path = directory.path().join("external.wav");
            if expected != ScanPhase::Failed {
                write_pcm(&path);
            }
            let before_queue = core.state.queue.total;
            core.start_external_admission(vec![path], None, None)
                .unwrap();
            let id = core.active_external_admission.as_ref().unwrap().status.id;
            let (handle, requests) = task_wait::wait_handle(&core);
            let mut wait = Box::pin(handle.wait_task(TaskTarget::Scan { id }));
            assert!(wait.as_mut().now_or_never().is_none());
            core.business_event(dispatch::BusinessEvent::Request(
                requests.try_recv().unwrap(),
            ));
            assert!(wait.as_mut().now_or_never().is_none());
            if shutdown {
                core.shutdown_external_admission();
            } else {
                if expected == ScanPhase::Cancelled {
                    core.cancel_external_admission(id).unwrap();
                    assert!(wait.as_mut().now_or_never().is_none());
                }
                finish_admission(&mut core);
            }
            let TaskOutcome::Scan(status) = futures::executor::block_on(wait).unwrap() else {
                panic!("Expected media task outcome");
            };
            assert_eq!(status.id, id);
            assert_eq!(status.progress.phase, expected);
            assert_eq!(
                core.shared
                    .read()
                    .system
                    .scan_progress
                    .as_ref()
                    .unwrap()
                    .phase,
                expected
            );
            assert_eq!(
                core.state.queue.total,
                before_queue + usize::from(expected == ScanPhase::Completed)
            );
        }
    }
}

#[test]
fn stale_queue_external_paths_command_recovers_before_accepting_task() {
    let (directory, mut core) = admission_fixture();
    let path = directory.path().join("external.wav");
    write_pcm_frames(&path, 1);
    let connection = committed_queue_scalar_refresh_failure(&mut core, directory.path());
    repair_queue_scalar(&connection);
    core.command(Command::QueueExternalPaths {
        paths: vec![path],
        index: None,
        expected_revision: None,
    })
    .unwrap();
    assert!(!core.queue_projection_stale);
    finish_admission(&mut core);
    assert_eq!(core.state.queue.total, 4);
    assert_eq!(
        core.store.queue_ids().unwrap().len(),
        core.state.queue.total
    );
}

#[test]
fn stale_queue_external_saving_rechecks_recovery_before_capturing_queue_snapshot() {
    for repaired in [false, true] {
        let (directory, mut core) = admission_fixture();
        let path = directory.path().join("external.wav");
        write_pcm_frames(&path, 1);
        core.store.set_queue_current(Some(1)).unwrap();
        core.refresh_queue_tracks().unwrap();
        core.start_external_admission(vec![path.clone()], None, None)
            .unwrap();
        let prepared = loop {
            let event = next_admission_event(&mut core);
            if matches!(&event, crate::core::admission::Event::Prepared(_)) {
                break event;
            }
            core.external_admission_event(event);
        };
        let id = core.active_external_admission.as_ref().unwrap().status.id;
        let connection = committed_queue_scalar_refresh_failure(&mut core, directory.path());
        if repaired {
            repair_queue_scalar(&connection);
        }
        core.external_admission_event(prepared);
        finish_admission(&mut core);
        let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
            panic!("expected retained admission result");
        };
        assert_eq!(status.library_committed, repaired);
        assert_eq!(
            status.progress.phase,
            if repaired {
                ScanPhase::Completed
            } else {
                ScanPhase::Failed
            }
        );
        assert_eq!(core.queue_projection_stale, !repaired);
        assert_eq!(
            core.store.track_count().unwrap(),
            if repaired { 4 } else { 3 }
        );
        assert_eq!(core.state.queue.current_id, Some(1));
        if repaired {
            assert_eq!(core.state.queue.total, 4);
            assert_eq!(
                core.store.queue_ids().unwrap().len(),
                core.state.queue.total
            );
            assert_eq!(&core.store.queue_ids().unwrap()[..3], &[1, 4, 5]);
        } else {
            assert!(
                core.store
                    .track_id_for_identity(crate::store::TrackIdentity::File(&path))
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn committed_external_queue_projection_failure_recovers_before_navigation() {
    for index in [None, Some(0)] {
        let (directory, mut core) = admission_fixture();
        core.state.queue.current_id = Some(5);
        core.save_queue().unwrap();
        core.refresh_queue_tracks().unwrap();
        let path = directory.path().join("external.wav");
        write_pcm_frames(&path, 1);
        let connection = fail_queue_projection_after(directory.path(), "INSERT");
        let expected_revision = index.map(|_| core.state.queue.revision);
        core.start_external_admission(vec![path], index, expected_revision)
            .unwrap();
        let id = core.active_external_admission.as_ref().unwrap().status.id;
        finish_admission(&mut core);
        let ViewResponse::ScanTask(status) = core.view(&Query::ScanTask { id }).unwrap() else {
            panic!("expected retained admission result");
        };
        assert!(status.library_committed);
        assert_eq!(status.progress.phase, ScanPhase::Failed);
        assert!(
            status
                .error
                .as_deref()
                .unwrap()
                .contains("refreshing its projection")
        );
        let commands = recording_audio(&mut core);
        assert!(core.navigate(Navigation::Previous).is_err());
        assert!(commands.try_recv().is_err());
        repair_queue_projection(&connection);
        core.navigate(Navigation::Previous).unwrap();
        assert_eq!(core.state.queue.current_id, Some(4));
        assert_eq!(core.state.queue.total, 6);
        assert_queue_saved(&core);
    }
}
