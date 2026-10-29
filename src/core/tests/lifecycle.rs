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
    let ack = futures::executor::block_on(control.shutdown_ack_async(|| 17));
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
    assert_eq!(shutdown.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[test]
fn shutdown_cancels_and_closes_an_async_scan_worker_before_retirement() {
    struct OnCancel(Box<dyn Fn() + Send + Sync>);

    impl std::task::Wake for OnCancel {
        fn wake(self: Arc<Self>) {
            (self.0)();
        }
    }

    for drop_owner in [false, true] {
        let (_directory, mut core) = fixture_with_database(Some(10.0));
        // Keep the executor alive until Drop has joined the scan worker.
        let _tasks = core._test_tasks.take().unwrap();
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

        // Attempt delivery synchronously at cancellation notification, before
        // the owner can advance to its next statement. No scheduling race.
        let (wake_tx, wake_rx) = bounded(1);
        let notify_sender = sender.clone();
        let notify_progress = progress.clone();
        let waker = std::task::Waker::from(Arc::new(OnCancel(Box::new(move || {
            let disconnected = matches!(
                notify_sender.try_send(ScanEvent::Progress {
                    id: 1,
                    progress: notify_progress.clone(),
                }),
                Err(async_channel::TrySendError::Closed(_))
            );
            wake_tx.try_send(disconnected).unwrap();
        }))));
        let notification = cancellation.clone();
        let mut cancelled = std::pin::pin!(notification.cancelled());
        assert!(
            cancelled
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );

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
            owner: crate::projection::MediaTaskOwner::Library,
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
        if drop_owner {
            drop(core);
        } else {
            core.shutdown_scan();
            assert!(core.active_scan.is_none());
            assert_eq!(
                core.scan_results.back().unwrap().progress.phase,
                ScanPhase::Cancelled
            );
        }
        assert!(wake_rx.try_recv().unwrap());
        assert_eq!(result_rx.try_recv().unwrap(), (true, true));
    }
}

#[test]
fn shutdown_drains_scan_saving_and_preserves_committed_facts() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
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
            .track_id_for_identity(crate::store::TrackIdentity::File(&source))
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
        .send_blocking(Request {
            operation: Operation::Command(Command::Pause),
            reply: None,
        })
        .unwrap();
    let ack = futures::executor::block_on(handle.operation_ack_async(
        Operation::Command(Command::Volume { value: 0.4 }),
        Duration::ZERO,
    ));
    assert_eq!(ack.status, AckStatus::NotAdmitted);
    assert!(matches!(
        requests.recv_blocking().unwrap().operation,
        Operation::Command(Command::Pause)
    ));
    assert!(requests.try_recv().is_err());
}

#[test]
fn admitted_command_timeout_leaves_owner_request_intact_without_replaying() {
    let (handle, _analysis, requests) = display_fixture();
    let ack = futures::executor::block_on(handle.operation_ack_async(
        Operation::Command(Command::Volume { value: 0.4 }),
        Duration::ZERO,
    ));
    assert_eq!(ack.status, AckStatus::Unknown);
    assert!(
        ack.error
            .as_deref()
            .unwrap()
            .contains("do not automatically retry")
    );
    // The owner can still receive and execute this exact request after timeout.
    let request = requests.recv_blocking().unwrap();
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
        drop(requests.recv_blocking().unwrap());
    });
    let ack = futures::executor::block_on(
        handle.operation_ack_async(Operation::Command(Command::Pause), REPLY_TIMEOUT),
    );
    assert_eq!(ack.status, AckStatus::Unknown);
    owner.join().unwrap();
}

#[test]
fn shutdown_acknowledgement_does_not_claim_runtime_teardown_is_finished() {
    let (control, _shutdown) = RuntimeControl::new();
    control.acknowledge(Ack::from_result(Ok(()), 9));
    assert!(futures::executor::block_on(control.shutdown_ack_async(|| 0)).is_success());
    assert!(!control.completion.state.lock().finished);
}

#[test]
fn query_startup_failure_retains_its_cause_after_final_persistence() {
    for invalid_config in [false, true] {
        let (_directory, mut core) = fixture();
        let _audio = recording_audio(&mut core);
        core.flush_config().unwrap();
        let path = core.state.configuration.config_path.clone();
        core.command(Command::Volume { value: 0.25 }).unwrap();
        if invalid_config {
            std::fs::write(&path, "[playback]\nvolume_percent = 101\n").unwrap();
        }
        // A memory-only store cannot start the independent read owner.
        core.store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (_sender, requests) = async_channel::bounded(1);
        let (control, shutdown) = RuntimeControl::new();
        core.run(requests, shutdown, &control);
        let error = control.completion.state.lock().error.take().unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("Starting read queries"));
        assert!(error.contains("file-backed library"));
        assert_eq!(error.contains("Saving configuration"), invalid_config);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        if invalid_config {
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "[playback]\nvolume_percent = 101\n"
            );
        } else {
            assert_eq!(Config::load(&path).unwrap().volume, 0.25);
        }
    }
}

#[test]
fn final_queue_failure_still_attempts_config_and_retains_all_failures() {
    for invalid_config in [false, true] {
        let (_directory, mut core) = fixture();
        let _audio = recording_audio(&mut core);
        core.flush_config().unwrap();
        let path = core.state.configuration.config_path.clone();
        if invalid_config {
            std::fs::write(&path, "[playback]\nvolume_percent = 101\n").unwrap();
        }
        core.command(Command::Volume { value: 0.25 }).unwrap();
        core.database_unavailable = true;
        let (handle, control, worker, _) = navigation_runtime(core);
        let ack = futures::executor::block_on(
            control.shutdown_ack_async(|| handle.publication_revision()),
        );
        assert!(ack.is_success());
        let core = worker.join().unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        let error = control.completion.state.lock().error.take().unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("Saving queue"));
        assert_eq!(error.contains("Saving configuration"), invalid_config);
        if !invalid_config {
            assert_eq!(Config::load(&path).unwrap().volume, 0.25);
        }
    }
}

#[test]
fn accepted_shutdown_reports_stop_failure_without_rejecting_the_request() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
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
    let ack =
        futures::executor::block_on(control.shutdown_ack_async(|| handle.publication_revision()));
    assert_eq!(ack.status, AckStatus::Succeeded);
    let core = worker.join().unwrap();
    assert!(core.state.system.shutting_down);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.state.system.last_error.is_some());
    let error = control.completion.state.lock().error.take().unwrap();
    assert!(format!("{error:#}").contains("activity unavailable"));
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
    let (_directory, mut core) = fixture_with_database(Some(10.0));
    let tasks = TaskRuntime::with_blocking_threads(1).unwrap();
    core.executor = tasks.handle.clone();
    core.workers = tasks.workers.clone();
    core._test_tasks = Some(tasks);
    let (handle, control, worker, _) = navigation_runtime(core);
    let (started, running) = bounded(1);
    let (release, released) = bounded(1);
    let workers = handle.workers().clone();
    let blocker = handle.executor.spawn(async move {
        workers
            .execute(move || {
                started.send(()).unwrap();
                let _ = released.recv();
            })
            .await
    });
    running.recv_timeout(Duration::from_secs(5)).unwrap();

    // More concurrent callers than physical pool slots still execute real Core
    // commands and SQL reads while that sole blocking slot is occupied.
    let requests = (0..32).map(|_| {
        let handle = handle.clone();
        async move {
            assert!(
                handle
                    .request_ack(Command::Volume { value: 0.42 })
                    .await
                    .is_success()
            );
            assert!(matches!(
                handle.query_for_gui(Query::Track { track_id: 1 }).await.result,
                Ok(rivu_protocol::response::ViewResponse::Track(Some(track))) if track.id == 1
            ));
        }
    });
    futures::executor::block_on(async {
        rivu_protocol::transport::timeout(
            Duration::from_secs(5),
            futures::future::join_all(requests),
        )
        .await
        .unwrap();
    });
    assert_eq!(handle.state.read().playback.volume, 0.42);

    let optimizing = handle.executor.spawn({
        let handle = handle.clone();
        async move { handle.request_ack(Command::OptimizeDatabase).await }
    });
    futures::executor::block_on(async {
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            loop {
                let response = handle.query_for_gui(Query::Track { track_id: 1 }).await;
                if matches!(&response.result, Err(QueryError::Message(error)) if error.contains("maintenance")) {
                    break;
                }
                futures_lite::future::yield_now().await;
            }
        }).await.unwrap();
    });
    assert!(!optimizing.is_finished());
    release.send(()).unwrap();
    futures::executor::block_on(blocker).unwrap();
    let ack = futures::executor::block_on(optimizing);
    assert!(ack.is_success());
    assert_eq!(
        ack.database_optimization,
        handle.state.read().system.database_optimization
    );
    assert!(ack.database_optimization.is_some());
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
        owner: crate::projection::MediaTaskOwner::Library,
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
fn owner_failure_retires_background_work_without_blocking_executor() {
    for during_request in [false, true] {
        let (_directory, mut core) = fixture();
        // Keep the shared runtime alive outside the owner being unwound.
        let _tasks = core._test_tasks.take().unwrap();
        let executor = core.executor.clone();
        let cancellation = library::ScanCancellation::default();
        let observed = cancellation.clone();
        let retired_scan = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scan_retired = retired_scan.clone();
        let scan = executor.spawn(async move {
            observed.cancelled().await;
            scan_retired.store(true, Ordering::Release);
        });
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
            owner: crate::projection::MediaTaskOwner::Library,
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
        let (requests, receiver) = async_channel::bounded(1);
        requests
            .try_send(Request {
                operation: Operation::Command(Command::Volume { value: 0.37 }),
                reply: None,
            })
            .unwrap();
        let (control, shutdown) = RuntimeControl::new();
        let (completed, completion) = bounded(1);
        let retired = retired_scan.clone();
        control.set_completion_callback(move || {
            completed.send(retired.load(Ordering::Acquire)).unwrap();
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
        futures::executor::block_on(ping_task);
        assert!(completion.recv_timeout(Duration::from_secs(5)).unwrap());
        assert!(retired_scan.load(Ordering::Acquire));
    }
}

#[test]
fn unrelated_album_query_failure_preserves_command_results_and_scan_identity() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    let original_error = format!(
        "{:#}",
        core.command(Command::Volume { value: -1.0 }).unwrap_err()
    );
    let (handle, control, worker, _publications) = navigation_runtime(core);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    // A missing album query target is a deterministic projection fault, not an
    // online writer whose catalog mutations Core is expected to discover.
    connection.execute_batch("DROP TABLE albums").unwrap();
    assert!(
        handle
            .query(Query::Albums {
                query: None,
                sort: Default::default(),
                offset: 0,
                limit: 1,
            })
            .result
            .is_err()
    );
    let accepted = futures::executor::block_on(handle.request_ack(Command::Volume { value: 0.37 }));
    assert_eq!(accepted.status, AckStatus::Succeeded);
    assert_eq!(handle.state.read().playback.volume, 0.37);
    assert!(handle.state.read().system.last_error.is_none());
    let rejected = futures::executor::block_on(handle.request_ack(Command::Volume { value: -1.0 }));
    assert_eq!(rejected.status, AckStatus::Failed);
    assert_eq!(rejected.error.as_deref(), Some(original_error.as_str()));
    let accepted = futures::executor::block_on(handle.request_ack(Command::Scan {
        paths: vec![directory.path().join("1.wav")],
        force: true,
    }));
    assert_eq!(accepted.status, AckStatus::Succeeded);
    let id = accepted
        .scan_id
        .expect("accepted scan must remain queryable");
    match handle.query(Query::ScanTask { id }).result.unwrap() {
        ViewResponse::ScanTask(task) => assert_eq!(task.id, id),
        other => panic!("Unexpected scan response: {other:?}"),
    }
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}
