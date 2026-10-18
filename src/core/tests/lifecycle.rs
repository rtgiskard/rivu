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
