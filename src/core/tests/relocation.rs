use super::*;

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
        .collect::<Vec<_>>();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records: records.into_iter().map(std::sync::Arc::new).collect(),
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

fn receive_relocation(core: &mut Core) -> RelocationFinished {
    futures::executor::block_on(async {
        rivu_protocol::transport::timeout(Duration::from_secs(5), core.relocation_rx.recv())
            .await
            .unwrap()
            .unwrap()
    })
}

fn settle_relocation(core: &mut Core) -> RelocationTaskStatus {
    let event = receive_relocation(core);
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
        track_ids: vec![source, 1, source],
    })
    .unwrap();
    let playlist_id = core
        .store
        .playlist_summary_page(Some("Preserved"), 0, 1)
        .unwrap()
        .rows[0]
        .id;
    let entries_before = core
        .store
        .playlist_entries_page(playlist_id, None, 0, MAX_QUERY_ROWS)
        .unwrap()
        .rows;
    let queue_before = core.store.queue_ids().unwrap();
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
    assert_eq!(core.store.queue_ids().unwrap(), queue_before);
    assert!(core.store.queued_track(source).unwrap().is_some());
    let entries_after = core
        .store
        .playlist_entries_page(playlist_id, None, 0, MAX_QUERY_ROWS)
        .unwrap()
        .rows;
    assert_eq!(entries_after.len(), entries_before.len());
    for (before, after) in entries_before.iter().zip(&entries_after) {
        assert_eq!(
            (after.id, after.position, after.track_id),
            (before.id, before.position, before.track_id)
        );
        if after.track_id == source {
            assert_eq!(after.path, target_path);
            assert!(!after.missing);
        } else {
            assert_eq!(after.path, before.path);
        }
    }
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
        let event = receive_relocation(&mut core);
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
    let event = receive_relocation(&mut core);
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
            let queue_id = *core.store.queue_ids().unwrap().last().unwrap();
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
    let event = receive_relocation(&mut core);
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
        let event = receive_relocation(&mut core);
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
                records: (vec![ScanRecord {
                    path: known.path,
                    size: known.size,
                    modified_ns: known.modified_ns,
                    fingerprint: Some(
                        blake3::hash(b"different historical identity")
                            .to_hex()
                            .to_string(),
                    ),
                    media: known.media.unwrap(),
                }])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
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

#[test]
fn verification_panic_retires_the_task_without_committing_and_allows_retry() {
    let (_directory, mut core, source, target) = relocation_fixture();
    let id = core.next_relocation_id;
    core.next_relocation_id += 1;
    let prepared = core.store.prepare_relocation(source, target).unwrap();
    let status = RelocationTaskStatus {
        id,
        source_id: source,
        target_id: target,
        phase: RelocationPhase::Verifying,
        error: None,
    };
    let worker =
        core.spawn_relocation_verification(id, async { panic!("injected verification panic") });
    core.active_relocation = Some(RelocationTask {
        status,
        prepared,
        cancellation: library::ScanCancellation::default(),
        worker: Some(worker),
    });
    let result = settle_relocation(&mut core);
    assert_eq!(result.phase, RelocationPhase::Failed);
    assert!(result.error.as_deref().unwrap().contains("panicked"));
    assert!(core.track(source).unwrap().missing);
    core.relocate_track(source, target).unwrap();
    assert_eq!(
        settle_relocation(&mut core).phase,
        RelocationPhase::Completed
    );
}

#[test]
fn cancellation_keeps_relocation_admission_until_the_async_worker_retires() {
    let (_directory, mut core, source, target) = relocation_fixture();
    let id = core.next_relocation_id;
    core.next_relocation_id += 1;
    let prepared = core.store.prepare_relocation(source, target).unwrap();
    let (started, observe_started) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let worker = core.spawn_relocation_verification(id, async move {
        let _ = started.send(());
        let _ = released.await;
        Err(anyhow::anyhow!("released verification"))
    });
    core.active_relocation = Some(RelocationTask {
        status: RelocationTaskStatus {
            id,
            source_id: source,
            target_id: target,
            phase: RelocationPhase::Verifying,
            error: None,
        },
        prepared,
        cancellation: library::ScanCancellation::default(),
        worker: Some(worker),
    });
    futures::executor::block_on(observe_started).unwrap();
    core.cancel_relocation(id).unwrap();
    assert!(core.active_relocation.is_some());
    assert!(core.relocation_rx.try_recv().is_err());
    assert!(core.relocate_track(source, target).is_err());
    release.send(()).unwrap();
    assert_eq!(
        settle_relocation(&mut core).phase,
        RelocationPhase::Cancelled
    );
    assert!(core.track(source).unwrap().missing);
    core.relocate_track(source, target).unwrap();
    assert_eq!(
        settle_relocation(&mut core).phase,
        RelocationPhase::Completed
    );
}

#[test]
fn relocation_waits_for_owner_commit_on_success_failure_cancellation_and_shutdown() {
    for expected in [
        RelocationPhase::Completed,
        RelocationPhase::Failed,
        RelocationPhase::Cancelled,
    ] {
        for shutdown in [false, true] {
            if shutdown && expected != RelocationPhase::Cancelled {
                continue;
            }
            let (_directory, mut core, source, target) = relocation_fixture();
            core.relocate_track(source, target).unwrap();
            let id = core.active_relocation.as_ref().unwrap().status.id;
            let event = receive_relocation(&mut core);
            let (handle, requests) = task_wait::wait_handle(&core);
            let mut wait = Box::pin(handle.wait_task(TaskTarget::Relocation { id }));
            assert!(wait.as_mut().now_or_never().is_none());
            core.business_event(dispatch::BusinessEvent::Request(
                requests.try_recv().unwrap(),
            ));
            assert!(wait.as_mut().now_or_never().is_none());
            match expected {
                RelocationPhase::Failed => {
                    std::fs::write(core.track(target).unwrap().path, b"changed target").unwrap();
                }
                RelocationPhase::Cancelled => core.cancel_relocation(id).unwrap(),
                _ => {}
            }
            if shutdown {
                core.shutdown_relocation();
                assert!(wait.as_mut().now_or_never().is_none());
                core.publish();
            } else {
                core.relocation_event(event);
            }
            let TaskOutcome::Relocation(status) = futures::executor::block_on(wait).unwrap() else {
                panic!("Expected relocation outcome");
            };
            assert_eq!(status.id, id);
            assert_eq!(status.phase, expected);
            assert_eq!(
                core.shared
                    .read()
                    .system
                    .relocation_task
                    .as_ref()
                    .unwrap()
                    .phase,
                expected
            );
            let mut retained = Box::pin(handle.wait_task(TaskTarget::Relocation { id }));
            assert!(retained.as_mut().now_or_never().is_none());
            core.business_event(dispatch::BusinessEvent::Request(
                requests.try_recv().unwrap(),
            ));
            assert!(matches!(retained.as_mut().now_or_never(),
                Some(Ok(TaskOutcome::Relocation(status))) if status.phase == expected));
        }
    }
}
