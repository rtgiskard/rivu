use super::*;

#[test]
fn gui_snapshot_skips_unchanged_revision_and_projects_current_state() {
    let (handle, _worker, _receiver) = display_fixture();
    let initial = handle.gui_snapshot();
    assert!(
        handle
            .gui_snapshot_if_changed(initial.system.revision)
            .is_none()
    );

    {
        let mut state = handle.state.write();
        state.system.revision = state.system.revision.wrapping_add(1);
        state.system.shutting_down = true;
    }
    let changed = handle
        .gui_snapshot_if_changed(initial.system.revision)
        .expect("changed state is projected");
    assert_eq!(
        changed.system.revision,
        initial.system.revision.wrapping_add(1)
    );
    assert!(changed.system.shutting_down);
    assert!(!initial.system.shutting_down);
    assert!(
        handle
            .gui_snapshot_if_changed(changed.system.revision)
            .is_none()
    );
}

fn display_wait(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < deadline, "display transition timed out");
        thread::yield_now();
    }
}

fn display_samples(
    producer: &mut ringbuf::HeapProd<rivu_audio::analysis::TapFrame>,
    worker: &rivu_audio::analysis::AnalysisWorker,
    demand_token: u64,
    count: usize,
    samples: [f32; 2],
) {
    use ringbuf::traits::Producer;
    for _ in 0..count {
        assert!(
            producer
                .try_push(rivu_audio::analysis::TapFrame {
                    samples,
                    epoch: worker.control.epoch(),
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
        visualizer: rivu_audio::analysis::VisualizerDemand {
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
