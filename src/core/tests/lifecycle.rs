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
    let ack = wait_on(&executor(), control.shutdown_ack_async(|| 17));
    assert_eq!(ack.status, AckStatus::Unknown);
    assert_eq!(ack.revision, 17);
}

#[test]
fn lifecycle_shutdown_coalesces_without_business_queue_capacity() {
    let (control, mut shutdown) = RuntimeControl::new();
    for _ in 0..1000 {
        control.request_shutdown().unwrap();
    }
    assert_eq!(shutdown.try_recv(), Ok(()));
    assert_eq!(shutdown.try_recv(), Err(mpsc::error::TryRecvError::Empty));
}

#[test]
fn shutdown_cancels_and_closes_an_async_scan_worker_before_retirement() {
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
        .try_send(ScanEvent::Progress {
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
        worker: core.executor.spawn(async move {
            observed.cancelled().await;
            let disconnected = sender
                .send(ScanEvent::Finished(ScanFinished {
                    other_tracks_check: false,
                    id: 1,
                    result: Ok(ScanResult::default()),
                    import: None,
                    progress,
                }))
                .await
                .is_err();
            result_tx
                .try_send((disconnected, observed.is_cancelled()))
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
    let (handle, _analysis, mut requests) = display_fixture();
    handle
        .sender
        .blocking_send(Request {
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
        requests.blocking_recv().unwrap().operation,
        Operation::Command(Command::Pause)
    ));
    assert!(requests.try_recv().is_err());
}

#[test]
fn admitted_command_timeout_leaves_owner_request_intact_without_replaying() {
    let (handle, _analysis, mut requests) = display_fixture();
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
    let request = requests.blocking_recv().unwrap();
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
    let (handle, _analysis, mut requests) = display_fixture();
    let owner = thread::spawn(move || {
        drop(requests.blocking_recv().unwrap());
    });
    let ack = handle.operation_ack(Operation::Command(Command::Pause), REPLY_TIMEOUT);
    assert_eq!(ack.status, AckStatus::Unknown);
    owner.join().unwrap();
}

#[test]
fn shutdown_acknowledgement_does_not_claim_runtime_teardown_is_finished() {
    let (control, _shutdown) = RuntimeControl::new();
    control.acknowledge(Ack::from_result(Ok(()), 9));
    assert!(wait_on(&executor(), control.shutdown_ack_async(|| 0)).is_success());
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
    let ack = wait_on(
        handle.executor(),
        control.shutdown_ack_async(|| handle.revision()),
    );
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

#[test]
fn async_replies_and_admission_do_not_need_a_blocking_pool_slot() {
    let (_directory, mut core) = fixture_with_database(Some(10.0), true);
    let tasks = TaskRuntime::with_blocking_threads(1).unwrap();
    core.executor = tasks.handle.clone();
    core._test_tasks = Some(tasks);
    let (handle, control, worker, _) = navigation_runtime(core);
    let (started, running) = bounded(1);
    let (release, released) = bounded(1);
    let blocker = handle.executor.spawn_blocking(move || {
        started.send(()).unwrap();
        let _ = released.recv();
    });
    running.recv_timeout(Duration::from_secs(5)).unwrap();

    // More concurrent callers than physical pool slots still execute real Core
    // commands and SQL reads while that sole blocking slot is occupied.
    let requests = (0..32).map(|_| {
        let handle = handle.clone();
        async move {
            assert!(
                handle
                    .request_ack_async(Command::Volume { value: 0.42 })
                    .await
                    .is_success()
            );
            assert!(matches!(
                handle.query_async(Query::Track { track_id: 1 }).await.result,
                Ok(crate::response::ViewResponse::Track(Some(track))) if track.id == 1
            ));
        }
    });
    wait_on(handle.executor(), async {
        tokio::time::timeout(Duration::from_secs(5), futures::future::join_all(requests))
            .await
            .unwrap();
    });
    assert_eq!(handle.state.read().playback.volume, 0.42);

    let optimizing = handle.executor.spawn({
        let handle = handle.clone();
        async move { handle.request_ack_async(Command::OptimizeDatabase).await }
    });
    wait_on(handle.executor(), async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = handle.query_async(Query::Track { track_id: 1 }).await;
                if matches!(&response.result, Err(QueryError::Message(error)) if error.contains("maintenance")) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
    });
    assert!(!optimizing.is_finished());
    release.send(()).unwrap();
    futures::executor::block_on(blocker).unwrap();
    assert!(
        futures::executor::block_on(optimizing)
            .unwrap()
            .is_success()
    );
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn preparation_panic_publishes_a_terminal_failure_and_releases_scan_admission() {
    let (directory, mut core) = fixture();
    let id = core.next_scan_id;
    let worker = core.spawn_scan_preparation(id, async { panic!("injected preparation panic") });
    core.active_scan = Some(ScanTask {
        status: ScanTaskStatus {
            id,
            progress: ScanProgress {
                phase: ScanPhase::Discovering,
                processed: 0,
                total: None,
                errors: 0,
                path: None,
            },
            message: String::new(),
            library_committed: false,
            playlist_id: None,
            error: None,
        },
        cancellation: library::ScanCancellation::default(),
        worker,
        pending_save: None,
    });
    let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    core.scan_event(event);
    assert!(core.active_scan.is_none());
    let result = core.scan_results.back().unwrap();
    assert_eq!(result.progress.phase, ScanPhase::Failed);
    assert!(!result.library_committed);
    assert!(result.error.as_deref().unwrap().contains("panicked"));
    core.scan(vec![directory.path().to_path_buf()], true)
        .unwrap();
    core.shutdown_scan();
}

#[test]
fn owner_failure_retires_background_work_without_blocking_tokio() {
    for during_request in [false, true] {
        let (_directory, mut core) = fixture();
        // Keep the shared runtime alive outside the owner being unwound.
        let _tasks = core._test_tasks.take().unwrap();
        let executor = core.executor.clone();
        let cancellation = library::ScanCancellation::default();
        let observed = cancellation.clone();
        let scan = executor.spawn(async move { observed.cancelled().await });
        let scan_retired = scan.abort_handle();
        core.active_scan = Some(ScanTask {
            status: ScanTaskStatus {
                id: 1,
                progress: ScanProgress {
                    phase: ScanPhase::Discovering,
                    processed: 0,
                    total: None,
                    errors: 0,
                    path: None,
                },
                message: String::new(),
                library_committed: false,
                playlist_id: None,
                error: None,
            },
            cancellation,
            worker: scan,
            pending_save: None,
        });
        let (entered, entering) = bounded(1);
        let (release, released) = bounded(1);
        *core.wakeup.write() = Some(Arc::new(move || {
            entered.send(()).unwrap();
            released.recv().unwrap();
            panic!("injected Core owner failure");
        }));
        let (requests, receiver) = mpsc::channel(1);
        requests
            .try_send(Request {
                operation: Operation::Command(Command::Volume { value: 0.37 }),
                reply: None,
            })
            .unwrap();
        let (control, shutdown) = RuntimeControl::new();
        let (completed, completion) = bounded(1);
        let retired = scan_retired.clone();
        control.set_completion_callback(move || {
            completed.send(retired.is_finished()).unwrap();
        });
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            let _completion = CompletionGuard(worker_control.clone());
            let mut core = core;
            if during_request {
                core.run(receiver, shutdown, &worker_control);
            } else {
                core.initialize();
            }
        });
        entering.recv_timeout(Duration::from_secs(5)).unwrap();
        let (ping, pong) = bounded(1);
        let ping_task = executor.spawn(async move {
            ping.send(()).unwrap();
        });
        let responsive = pong.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        assert!(worker.join().is_err());
        responsive.unwrap();
        wait_on(&executor, ping_task).unwrap();
        assert!(completion.recv_timeout(Duration::from_secs(5)).unwrap());
        assert!(scan_retired.is_finished());
    }
}
