use super::*;

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
    assert_eq!(ack.status, AckStatus::Unknown);
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

#[test]
fn shutdown_cancels_and_disconnects_a_backpressured_scan_worker() {
    let (_directory, mut core) = fixture_with_database(Some(10.0), true);
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
            library_committed: false,
            playlist_id: None,
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
        pending_save: None,
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
fn shutdown_drains_scan_saving_and_preserves_committed_facts() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    let source = directory.path().join("saved-on-shutdown.wav");
    write_pcm(&source);
    core.scan(vec![source.clone()], true).unwrap();
    while core.active_scan.as_ref().unwrap().status.progress.phase != ScanPhase::Saving {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        core.scan_event(event);
    }
    let id = core.active_scan.as_ref().unwrap().status.id;
    core.shutdown_scan();
    assert!(core.active_scan.is_none());
    let status = core.scan_results.back().unwrap();
    assert_eq!(status.id, id);
    assert_eq!(status.progress.phase, ScanPhase::Completed);
    assert!(status.library_committed);
    assert_eq!(status.playlist_id, None);
    assert!(status.error.is_none());
    assert!(
        core.store
            .track_id_for_source(&source, None)
            .unwrap()
            .is_some()
    );
    assert_eq!(core.state.library.track_total, 5);
}

#[test]
fn saturated_command_queue_reports_not_admitted_without_inserting_the_command() {
    let (handle, _analysis, requests) = display_fixture();
    handle
        .sender
        .send(Request {
            operation: Operation::Command(Command::Pause),
            reply: None,
        })
        .unwrap();
    let ack = handle.operation_ack(
        Operation::Command(Command::Volume { value: 0.4 }),
        Duration::ZERO,
    );
    assert_eq!(ack.status, AckStatus::NotAdmitted);
    assert!(matches!(
        requests.recv().unwrap().operation,
        Operation::Command(Command::Pause)
    ));
    assert!(requests.try_recv().is_err());
}

#[test]
fn admitted_command_timeout_leaves_owner_request_intact_without_replaying() {
    let (handle, _analysis, requests) = display_fixture();
    let ack = handle.operation_ack(
        Operation::Command(Command::Volume { value: 0.4 }),
        Duration::ZERO,
    );
    assert_eq!(ack.status, AckStatus::Unknown);
    assert!(
        ack.error
            .as_deref()
            .unwrap()
            .contains("do not automatically retry")
    );
    // The owner can still receive and execute this exact request after timeout.
    let request = requests.recv().unwrap();
    assert!(
        matches!(request.operation, Operation::Command(Command::Volume { value }) if value == 0.4)
    );
    assert!(
        request
            .reply
            .unwrap()
            .send(CoreResponse::Ack(Ack::from_result(Ok(()), 1)))
            .is_err()
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn owner_disconnect_after_admission_reports_unknown_not_rejection() {
    let (handle, _analysis, requests) = display_fixture();
    let owner = thread::spawn(move || {
        drop(requests.recv().unwrap());
    });
    let ack = handle.operation_ack(Operation::Command(Command::Pause), REPLY_TIMEOUT);
    assert_eq!(ack.status, AckStatus::Unknown);
    owner.join().unwrap();
}

#[test]
fn shutdown_acknowledgement_does_not_claim_runtime_teardown_is_finished() {
    let (control, _shutdown) = RuntimeControl::new();
    control.acknowledge(Ack::from_result(Ok(()), 9));
    assert!(control.shutdown_ack(|| 0).is_success());
    assert!(!control.completion.state.lock().finished);
}

#[test]
fn accepted_shutdown_reports_stop_failure_without_rejecting_the_request() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    let _audio = recording_audio(&mut core);
    playing(&mut core, 1);
    progress(&mut core, 3.0, 3.0);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "UPDATE tracks SET last_played=NULL WHERE id=1;
         CREATE TRIGGER reject_shutdown_activity BEFORE UPDATE OF last_played ON tracks
         BEGIN SELECT RAISE(FAIL,'activity unavailable'); END;",
        )
        .unwrap();
    let (handle, control, worker, _) = navigation_runtime(core);
    let ack = control.shutdown_ack(|| handle.revision());
    assert_eq!(ack.status, AckStatus::Succeeded);
    let core = worker.join().unwrap();
    assert!(core.state.system.shutting_down);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.state.system.last_error.is_some());
    let (count, last_played) = connection
        .query_row(
            "SELECT play_count,last_played FROM tracks WHERE id=1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(last_played, None);
}
