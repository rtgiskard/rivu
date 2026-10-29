use super::*;
use crate::core::task_wait::MAX_TASK_WAITERS;

pub(super) fn wait_handle(core: &Core) -> (CoreHandle, mpsc::Receiver<Request>) {
    let (sender, requests) = mpsc::bounded(MAX_TASK_WAITERS + 1);
    let handle = CoreHandle {
        state: Arc::clone(&core.shared),
        analysis: Arc::clone(&core.engine.analysis),
        analysis_control: Arc::clone(&core.engine.analysis_control),
        waveform: Arc::clone(&core.engine.waveform),
        sender,
        executor: Arc::clone(&core.executor),
        workers: Arc::clone(&core.workers),
        revision_updates: Arc::clone(&core.revision_updates),
        wakeup: Arc::clone(&core.wakeup),
        task_waiters: Arc::clone(&core.task_waiters),
        query_timeout: None,
        _test_tasks: None,
    };
    (handle, requests)
}

fn dispatch_wait(core: &mut Core, requests: &mpsc::Receiver<Request>) {
    assert!(core.business_event(dispatch::BusinessEvent::Request(
        requests.try_recv().unwrap(),
    )));
}

fn finish_scan(core: &mut Core) {
    while core.active_scan.is_some() {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        core.scan_event(event);
    }
}

fn processing_completion(core: &mut Core) -> crate::processing::Event {
    loop {
        let event = core
            .processing
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        if matches!(event, crate::processing::Event::Finished { .. }) {
            return event;
        }
        core.processing_event(event);
    }
}

#[test]
fn registration_and_completion_are_serialized_in_either_order() {
    for complete_before_dispatch in [false, true] {
        let (directory, mut core) = fixture();
        let path = directory.path().join("1.wav");
        write_pcm(&path);
        core.scan(vec![path], false).unwrap();
        let id = core.active_scan.as_ref().unwrap().status.id;
        let (handle, requests) = wait_handle(&core);
        let mut wait = Box::pin(handle.wait_task(TaskTarget::Scan { id }));
        assert!(wait.as_mut().now_or_never().is_none());
        if complete_before_dispatch {
            finish_scan(&mut core);
        }
        dispatch_wait(&mut core, &requests);
        if !complete_before_dispatch {
            assert!(wait.as_mut().now_or_never().is_none());
            finish_scan(&mut core);
        }
        let TaskOutcome::Scan(status) = futures::executor::block_on(wait).unwrap() else {
            panic!("Expected scan outcome");
        };
        assert_eq!(status.id, id);
        assert_eq!(status.progress.phase, ScanPhase::Completed);
        assert!(status.library_committed);
        assert_eq!(
            handle
                .state(StateSections {
                    system: true,
                    ..Default::default()
                })
                .system
                .unwrap()
                .scan_progress
                .unwrap()
                .phase,
            ScanPhase::Completed,
        );
    }
}

#[test]
fn worker_completion_does_not_return_before_scan_owner_commits_state() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    core.scan(vec![path], false).unwrap();
    let id = core.active_scan.as_ref().unwrap().status.id;
    let saved = loop {
        let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if matches!(event, ScanEvent::Saved(_)) {
            break event;
        }
        core.scan_event(event);
    };
    let (handle, requests) = wait_handle(&core);
    let mut wait = Box::pin(handle.wait_task(TaskTarget::Scan { id }));
    assert!(wait.as_mut().now_or_never().is_none());
    dispatch_wait(&mut core, &requests);
    assert!(wait.as_mut().now_or_never().is_none());
    assert_eq!(
        core.active_scan.as_ref().unwrap().status.progress.phase,
        ScanPhase::Saving
    );
    core.scan_event(saved);
    let TaskOutcome::Scan(status) = futures::executor::block_on(wait).unwrap() else {
        panic!("Expected scan outcome");
    };
    assert!(status.library_committed);
    assert_eq!(status.progress.phase, ScanPhase::Completed);
    assert_eq!(
        core.shared
            .read()
            .system
            .scan_progress
            .as_ref()
            .unwrap()
            .phase,
        ScanPhase::Completed
    );
}

#[test]
fn waiter_capacity_is_bounded_and_drop_recovers_it_without_owner_work() {
    for activated in [false, true] {
        let (directory, mut core) = fixture();
        let path = directory.path().join("1.wav");
        write_pcm(&path);
        core.scan(vec![path], false).unwrap();
        let id = core.active_scan.as_ref().unwrap().status.id;
        let (handle, requests) = wait_handle(&core);
        let mut waits = Vec::new();
        for _ in 0..MAX_TASK_WAITERS {
            let mut wait = Box::pin(handle.wait_task(TaskTarget::Scan { id }));
            assert!(wait.as_mut().now_or_never().is_none());
            waits.push(wait);
            if activated {
                dispatch_wait(&mut core, &requests);
            }
        }
        assert_eq!(
            futures::executor::block_on(handle.wait_task(TaskTarget::Scan { id })).unwrap_err(),
            QueryError::Message("Task wait capacity exhausted".into()),
        );
        drop(waits);
        // No dispatch, completion or periodic sweep intervenes in reclamation.
        let mut recovered = Box::pin(handle.wait_task(TaskTarget::Scan { id }));
        assert!(recovered.as_mut().now_or_never().is_none());
        if !activated {
            // The cancelled requests remain queued. Dispatch their old identities
            // before the newly reserved waiter; they must not resurrect or steal
            // its recovered capacity before the task completes.
            for _ in 0..MAX_TASK_WAITERS {
                dispatch_wait(&mut core, &requests);
            }
        }
        dispatch_wait(&mut core, &requests);
        assert!(recovered.as_mut().now_or_never().is_none());
        finish_scan(&mut core);
        assert!(matches!(futures::executor::block_on(recovered),
            Ok(TaskOutcome::Scan(status)) if status.progress.phase == ScanPhase::Completed));
    }
}

#[test]
fn dropping_an_active_wait_does_not_cancel_processing() {
    let (directory, mut core) = fixture();
    processing::source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    let id = core.state.system.processing.as_ref().unwrap().id;
    let (handle, requests) = wait_handle(&core);
    let mut wait = Box::pin(handle.wait_task(TaskTarget::Processing { id }));
    assert!(wait.as_mut().now_or_never().is_none());
    dispatch_wait(&mut core, &requests);
    drop(wait);
    assert!(
        !core
            .processing
            .active
            .as_ref()
            .unwrap()
            .cancelled
            .load(Ordering::Acquire)
    );
    let event = processing_completion(&mut core);
    core.processing_event(event);
    assert_eq!(
        core.processing_status(id).unwrap().phase,
        ProcessingPhase::Complete
    );
    // Retained terminal reads complete in the dispatch turn, with no next event.
    let mut terminal = Box::pin(handle.wait_task(TaskTarget::Processing { id }));
    assert!(terminal.as_mut().now_or_never().is_none());
    dispatch_wait(&mut core, &requests);
    assert!(
        matches!(terminal.as_mut().now_or_never(), Some(Ok(TaskOutcome::Processing(status)))
        if status.phase == ProcessingPhase::Complete)
    );
}

#[test]
fn processing_worker_phase_and_result_cannot_publish_a_terminal_reply_early() {
    let (directory, mut core) = fixture();
    processing::source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    let id = core.state.system.processing.as_ref().unwrap().id;
    let completion = processing_completion(&mut core);
    // A terminal-looking worker progress report is not the owner's acceptance.
    core.processing_event(crate::processing::Event::Progress {
        id,
        completed: 4096,
        total: Some(4096),
        phase: ProcessingPhase::Complete,
    });
    let (handle, requests) = wait_handle(&core);
    let mut wait = Box::pin(handle.wait_task(TaskTarget::Processing { id }));
    assert!(wait.as_mut().now_or_never().is_none());
    dispatch_wait(&mut core, &requests);
    assert!(wait.as_mut().now_or_never().is_none());
    // Owner-side source revalidation rejects the completed worker's output.
    std::fs::write(directory.path().join("1.wav"), b"replacement").unwrap();
    core.processing_event(completion);
    let TaskOutcome::Processing(status) = futures::executor::block_on(wait).unwrap() else {
        panic!("Expected processing outcome");
    };
    assert_eq!(status.phase, ProcessingPhase::Failed);
    assert!(status.outputs.is_none());
    assert_eq!(
        core.shared.read().system.processing.as_ref().unwrap().phase,
        ProcessingPhase::Failed
    );
}

#[test]
fn shutdown_publishes_processing_cancellation_before_reply() {
    let (directory, mut core) = fixture();
    processing::source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    let id = core.state.system.processing.as_ref().unwrap().id;
    let (handle, requests) = wait_handle(&core);
    let mut wait = Box::pin(handle.wait_task(TaskTarget::Processing { id }));
    assert!(wait.as_mut().now_or_never().is_none());
    dispatch_wait(&mut core, &requests);
    core.shutdown_processing();
    assert!(wait.as_mut().now_or_never().is_none());
    core.publish();
    assert!(
        matches!(futures::executor::block_on(wait), Ok(TaskOutcome::Processing(status))
        if status.phase == ProcessingPhase::Cancelled)
    );
    assert_eq!(
        core.shared.read().system.processing.as_ref().unwrap().phase,
        ProcessingPhase::Cancelled
    );
}

#[test]
fn unknown_and_expired_waits_are_errors_and_core_drop_retires_pending_waits() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("1.wav");
    write_pcm(&path);
    core.scan(vec![path], false).unwrap();
    finish_scan(&mut core);
    let status = core.scan_results.back().unwrap().clone();
    for id in 2..=MAX_RETAINED_SCAN_RESULTS as u64 + 1 {
        let mut retained = status.clone();
        retained.id = id;
        core.retain_scan_outcome(retained, crate::core::state::MediaTaskOwner::Library);
    }
    let (handle, requests) = wait_handle(&core);
    for (target, message) in [
        (TaskTarget::Scan { id: 1 }, "Unknown or expired scan task"),
        (
            TaskTarget::Relocation { id: 999 },
            "Unknown or expired relocation task",
        ),
        (
            TaskTarget::Processing { id: 999 },
            "Unknown or expired processing task",
        ),
    ] {
        let mut wait = Box::pin(handle.wait_task(target));
        assert!(wait.as_mut().now_or_never().is_none());
        dispatch_wait(&mut core, &requests);
        assert_eq!(
            futures::executor::block_on(wait).unwrap_err(),
            QueryError::Message(message.into())
        );
    }
    let mut undispatched = Box::pin(handle.wait_task(TaskTarget::Scan { id: 2 }));
    assert!(undispatched.as_mut().now_or_never().is_none());
    drop(core);
    assert!(futures::executor::block_on(undispatched).is_err());
    assert_eq!(
        futures::executor::block_on(handle.wait_task(TaskTarget::Scan { id: 2 })).unwrap_err(),
        QueryError::Message("Core is stopped".into()),
    );
}

#[test]
fn scan_failure_cancellation_and_shutdown_wake_registered_waits() {
    for expected in [ScanPhase::Failed, ScanPhase::Cancelled] {
        for shutdown in [false, true] {
            if shutdown && expected != ScanPhase::Cancelled {
                continue;
            }
            let (directory, mut core) = fixture();
            // Accepted preparation fails only when its worker opens the input.
            core.import_playlist(directory.path().join("missing.m3u"), "Missing".into())
                .unwrap();
            let id = core.active_scan.as_ref().unwrap().status.id;
            let (handle, requests) = wait_handle(&core);
            let mut wait = Box::pin(handle.wait_task(TaskTarget::Scan { id }));
            assert!(wait.as_mut().now_or_never().is_none());
            dispatch_wait(&mut core, &requests);
            assert!(wait.as_mut().now_or_never().is_none());
            if shutdown {
                core.shutdown_scan();
                assert!(wait.as_mut().now_or_never().is_none());
                core.publish();
            } else {
                if expected == ScanPhase::Cancelled {
                    core.cancel_scan(id).unwrap();
                    assert!(wait.as_mut().now_or_never().is_none());
                }
                finish_scan(&mut core);
            }
            let TaskOutcome::Scan(status) = futures::executor::block_on(wait).unwrap() else {
                panic!("Expected scan outcome");
            };
            assert_eq!(status.progress.phase, expected);
            assert!(!status.library_committed);
        }
    }
}
