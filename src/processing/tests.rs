use super::export::prepare_export;
use super::render::ProgressReporter;
use super::*;
use std::time::Duration;

fn samples(pcm: &audio::PcmAudio) -> Vec<f32> {
    let mut samples = vec![0.0; pcm.frames() as usize * usize::from(pcm.channels())];
    assert_eq!(pcm.read_frames(0, &mut samples), pcm.frames() as usize);
    samples
}

const PCM: [f32; 4] = [0.125, -0.25, 0.5, 0.0];

fn bounded(run: impl FnOnce() + Send + 'static) {
    let (completed, receive) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        run();
        completed.send(()).unwrap();
    });
    // A regression that blocks admission or shutdown must fail, not hang the suite.
    receive
        .recv_timeout(Duration::from_secs(10))
        .expect("processing admission or shutdown blocked");
    worker.join().unwrap();
}

fn next_event(service: &mut Service) -> Event {
    futures::executor::block_on(async {
        rivu_protocol::transport::timeout(Duration::from_secs(10), service.receiver.recv())
            .await
            .expect("processing worker did not report completion")
            .expect("processing events closed before completion")
    })
}

fn completed(service: &mut Service, track: &Track, config: &Arc<Config>) -> ProcessingResult {
    let status = service
        .start(
            track.clone(),
            config.clone(),
            ProcessingOperation::Denoise,
            None,
        )
        .unwrap();
    loop {
        match next_event(service) {
            Event::Progress { .. } => {}
            Event::Finished { id, result } => {
                assert_eq!(id, status.id);
                service.finish_task(service.active_id().unwrap()).unwrap();
                return *result.unwrap();
            }
            _ => panic!("unexpected processing event"),
        }
    }
}

fn fixture() -> (tempfile::TempDir, Service, Track, Arc<Config>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.wav");
    let mut writer = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for sample in PCM {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    let mut track = crate::artwork::tests::track(path);
    track.bits_per_sample = Some(32);
    let config = Arc::new(Config::default());
    let mut service = Service::new();
    let mut result = completed(&mut service, &track, &config);
    let stem = result.outputs.denoised.take().unwrap();
    result.outputs.vocals = Some(stem.clone());
    result.outputs.accompaniment = Some(stem);
    result.status.outputs = Some(ProcessingOutputs {
        vocals: true,
        accompaniment: true,
        ..Default::default()
    });
    let _ = service.retain_result(Box::new(result));
    service.select_audition(Some((1, 0)));
    (directory, service, track, config)
}

#[test]
fn progress_backpressure_coalesces_blocks_but_preserves_stages_and_terminal() {
    bounded(|| {
        let (sender, receiver) = async_channel::bounded(1);
        let (reported, burst) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut progress = ProgressReporter::new(&sender, 7);
            progress.report(ProcessingPhase::Rendering, 0, None);
            for completed in 1..=10_000 {
                progress.report(ProcessingPhase::Rendering, completed, None);
            }
            reported.send(()).unwrap();
            progress.report(ProcessingPhase::Processing, 0, Some(10_000));
            for completed in 1..=10_000 {
                progress.report(ProcessingPhase::Processing, completed, Some(10_000));
            }
            sender
                .send_blocking(Event::Finished {
                    id: 7,
                    result: Err(anyhow::anyhow!("consumer-visible failure")),
                })
                .unwrap();
        });
        burst.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(receiver.len(), 1);
        assert!(matches!(
            receiver.recv_blocking().unwrap(),
            Event::Progress {
                id: 7,
                phase: ProcessingPhase::Rendering,
                ..
            }
        ));
        assert!(matches!(
            receiver.recv_blocking().unwrap(),
            Event::Progress {
                id: 7,
                phase: ProcessingPhase::Processing,
                ..
            }
        ));
        loop {
            match receiver.recv_blocking().unwrap() {
                Event::Progress {
                    id: 7,
                    phase: ProcessingPhase::Processing,
                    ..
                } => {}
                Event::Finished { id: 7, result } => {
                    assert_eq!(
                        result.err().unwrap().to_string(),
                        "consumer-visible failure"
                    );
                    break;
                }
                _ => panic!("unexpected processing event"),
            }
        }
        worker.join().unwrap();
    });
}

#[test]
fn shutdown_releases_worker_with_a_full_progress_channel() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        for completed in 0..service.sender.capacity().unwrap() {
            service
                .sender
                .try_send(Event::Progress {
                    id: 0,
                    phase: ProcessingPhase::Rendering,
                    completed: completed as u64,
                    total: None,
                })
                .unwrap();
        }
        service
            .start(track, config, ProcessingOperation::Denoise, None)
            .unwrap();
        service.shutdown();
        assert!(service.active_id().is_none());
        assert!(service.receiver.is_closed());
    });
}

#[test]
fn shutdown_closes_processing_delivery_and_retires_unpublished_outputs() {
    let (_directory, mut service, track, config) = fixture();
    let buffered = completed(&mut service, &track, &config);
    let blocked = completed(&mut service, &track, &config);
    let retained_bytes = service.results()[0]
        .outputs
        .vocals
        .as_ref()
        .unwrap()
        .retained_bytes();
    service
        .sender
        .try_send(Event::Finished {
            id: buffered.status.id,
            result: Ok(Box::new(buffered)),
        })
        .ok()
        .unwrap();
    while service.sender.len() < service.sender.capacity().unwrap() {
        service
            .sender
            .try_send(Event::Progress {
                id: 4,
                completed: 0,
                total: None,
                phase: ProcessingPhase::Processing,
            })
            .ok()
            .unwrap();
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&cancelled);
    let sender = service.sender.clone();
    let (sent, delivery) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let rejected = sender
            .send_blocking(Event::Finished {
                id: blocked.status.id,
                result: Ok(Box::new(blocked)),
            })
            .is_err();
        sent.send(rejected).unwrap();
        released.recv().unwrap();
    });
    service.active = Some(ActiveTask {
        id: 4,
        cancelled,
        worker,
    });
    let (finished, completion) = std::sync::mpsc::channel();
    let shutdown = thread::spawn(move || {
        service.shutdown();
        assert!(service.sender.is_closed());
        assert!(service.receiver.try_recv().is_err());
        assert_eq!(service.memory_bytes(), retained_bytes);
        finished.send(()).unwrap();
        service
    });
    let rejected = delivery.recv_timeout(Duration::from_secs(5)).unwrap();
    let waiting_for_retirement = completion.try_recv().is_err();
    let cancelled = observed.load(Ordering::Acquire);
    release.send(()).unwrap();
    let service = shutdown.join().unwrap();
    completion.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rejected, "shutdown accepted a new terminal delivery");
    assert!(cancelled);
    assert!(
        waiting_for_retirement,
        "shutdown returned before worker retirement"
    );
    assert_eq!(service.memory_bytes(), retained_bytes);
    assert!(service.active_id().is_none());
}

#[test]
fn standalone_profile_rejects_empty_frame_and_out_of_source_selections() {
    let (_directory, mut service, track, _) = fixture();
    let mut config = Config::default();
    config.denoise.mode = rivu_dsp::denoise::DenoiseMode::Profile;
    config.denoise.fft_size = 256;
    let config = Arc::new(config);
    for selection in [
        NoiseSelection {
            start_seconds: 0.0,
            end_seconds: 0.5 / 8_000.0,
        },
        NoiseSelection {
            start_seconds: 0.0,
            end_seconds: 5.0 / 8_000.0,
        },
        NoiseSelection {
            start_seconds: 5.0 / 8_000.0,
            end_seconds: 6.0 / 8_000.0,
        },
    ] {
        service
            .start(
                track.clone(),
                Arc::clone(&config),
                ProcessingOperation::Denoise,
                Some(selection),
            )
            .unwrap();
        loop {
            if let Event::Finished { result, .. } = next_event(&mut service) {
                service.finish_task(service.active_id().unwrap()).unwrap();
                assert!(result.is_err(), "invalid selection must not publish PCM");
                break;
            }
        }
    }
}

fn graph_config(kinds: Vec<GraphNodeKind>) -> Arc<Config> {
    let mut config = Config::default();
    let graph = &mut config.processing_graph;
    graph.edges.clear();
    let mut from = GraphPort { node: 1, port: 0 };
    for (index, kind) in kinds.into_iter().enumerate() {
        let id = index as u64 + 3;
        graph.nodes.push(GraphNode {
            id,
            kind,
            position: [0.0; 2],
        });
        let to = GraphPort { node: id, port: 0 };
        graph.edges.push(GraphEdge { from, to });
        from = to;
        graph.next_id = id + 1;
    }
    graph.edges.push(GraphEdge {
        from,
        to: GraphPort { node: 2, port: 0 },
    });
    Arc::new(config)
}

fn completed_graph(
    service: &mut Service,
    track: &Track,
    config: Arc<Config>,
) -> Result<ProcessingResult> {
    service.start_graph(
        track.clone(),
        config,
        GraphRequest {
            target: GraphPort { node: 2, port: 0 },
            mode: GraphExecutionMode::Offline,
            export_path: None,
            source_run: None,
            cached_output: None,
        },
    )?;
    loop {
        match next_event(service) {
            Event::Progress { .. } | Event::GraphNode { .. } => {}
            Event::Finished { result, .. } => {
                service.finish_task(service.active_id().unwrap()).unwrap();
                return result.map(|result| *result);
            }
            _ => panic!("unexpected graph event"),
        }
    }
}

fn assert_graph_failure(
    service: &mut Service,
    track: &Track,
    config: Arc<Config>,
    failed_node: u64,
    expected_error: &str,
) {
    let task = service
        .start_graph(
            track.clone(),
            config,
            GraphRequest {
                target: GraphPort { node: 2, port: 0 },
                mode: GraphExecutionMode::Offline,
                export_path: None,
                source_run: None,
                cached_output: None,
            },
        )
        .unwrap();
    let mut processing_seen = false;
    let mut terminal_nodes = Vec::new();
    let task_error = loop {
        match next_event(service) {
            Event::Progress { id, .. } => assert_eq!(id, task.id),
            Event::GraphNode { id, status } => {
                assert_eq!(id, task.id);
                if status.node == failed_node && status.phase == ProcessingPhase::Processing {
                    processing_seen = true;
                }
                if matches!(
                    status.phase,
                    ProcessingPhase::Complete
                        | ProcessingPhase::Failed
                        | ProcessingPhase::Cancelled
                ) {
                    terminal_nodes.push(status);
                }
            }
            Event::Finished { id, result } => {
                assert_eq!(id, task.id);
                service.finish_task(service.active_id().unwrap()).unwrap();
                break format!(
                    "{:#}",
                    result.err().expect("graph task unexpectedly succeeded")
                );
            }
            _ => panic!("unexpected graph failure event"),
        }
    };
    assert!(task_error.contains(expected_error), "{task_error}");
    assert!(processing_seen, "failing node never entered Processing");
    assert_eq!(
        terminal_nodes.len(),
        1,
        "missing or duplicate terminal nodes"
    );
    let failed = &terminal_nodes[0];
    assert_eq!(failed.node, failed_node);
    assert_eq!(failed.phase, ProcessingPhase::Failed);
    assert_eq!(failed.error.as_deref(), Some(task_error.as_str()));
}

#[test]
fn offline_bypass_preserves_shared_causal_pcm_and_publication() {
    let (_directory, mut service, track, _) = fixture();
    let before = service.memory_bytes();
    let config = graph_config(vec![
        GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                ..Default::default()
            },
            implementation: EqImplementation::LinearPhase { taps: 65 },
        },
        GraphNodeKind::Equalizer {
            config: Default::default(),
            implementation: EqImplementation::Iir,
        },
        GraphNodeKind::Denoise {
            config: Default::default(),
            profile: None,
        },
    ]);
    let result = completed_graph(&mut service, &track, config).unwrap();
    let processed = result
        .graph_outputs
        .get(&GraphPort { node: 2, port: 0 })
        .unwrap();
    let context = processed.context.as_ref().unwrap();
    assert!(context.start_frame < 0);
    assert_eq!(processed.pcm.frames(), PCM.len() as u64);
    assert_eq!(result.graph_outputs.len(), 1);
    let crop = context
        .pcm
        .slice_frames(context.start_frame.unsigned_abs(), PCM.len() as u64)
        .unwrap();
    assert_eq!(samples(&processed.pcm), samples(&crop));
    assert_eq!(processed.pcm.retained_bytes(), context.pcm.retained_bytes());
    assert_eq!(
        service.memory_bytes(),
        before + context.pcm.retained_bytes()
    );
    for (actual, expected) in samples(&processed.pcm).into_iter().zip(PCM) {
        assert!((actual - expected).abs() < 1e-5);
    }
    assert_eq!(fs::read_dir(_directory.path()).unwrap().count(), 1);
}

#[test]
fn offline_off_denoise_keeps_hum_and_impulse_active_but_wet_zero_bypasses_both() {
    for mode in 0..3 {
        let (_directory, mut service, track, _) = fixture();
        let mut input = [0.125_f32; 64];
        input[32] = 0.875;
        let mut writer = hound::WavWriter::create(
            &track.path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for sample in input {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        let mut denoise = rivu_dsp::denoise::DenoiseConfig::default();
        match mode {
            0 => denoise.hum.enabled = true,
            1 => denoise.impulse.enabled = true,
            _ => {
                denoise.hum.enabled = true;
                denoise.impulse.enabled = true;
                denoise.wet = 0.0;
            }
        }
        let result = completed_graph(
            &mut service,
            &track,
            graph_config(vec![GraphNodeKind::Denoise {
                config: denoise.clone(),
                profile: None,
            }]),
        )
        .unwrap();
        let output = result
            .graph_outputs
            .get(&GraphPort { node: 2, port: 0 })
            .unwrap();
        let mut denoiser = rivu_dsp::denoise::Denoiser::new(8_000, 1, denoise, None).unwrap();
        let mut expected = Vec::new();
        denoiser
            .process_block(&input, &mut expected, &mut || false)
            .unwrap();
        denoiser.finish(&mut expected, &mut || false).unwrap();
        match mode {
            0 => assert_ne!(expected, input),
            1 => assert_eq!(expected, [0.125; 64]),
            _ => assert_eq!(expected, input),
        }
        assert_eq!(samples(&output.pcm), expected);
        assert_eq!(fs::read_dir(_directory.path()).unwrap().count(), 1);
    }
}

#[test]
fn offline_wet_zero_profile_still_rejects_out_of_source_selection() {
    let (_directory, mut service, track, _) = fixture();
    let config = graph_config(vec![GraphNodeKind::Denoise {
        config: rivu_dsp::denoise::DenoiseConfig {
            mode: rivu_dsp::denoise::DenoiseMode::Profile,
            wet: 0.0,
            fft_size: 256,
            ..Default::default()
        },
        profile: Some(NoiseSelection {
            start_seconds: 0.0,
            end_seconds: 1.0,
        }),
    }]);
    assert_graph_failure(&mut service, &track, config, 3, "outside the source");
}

#[test]
fn offline_non_profile_selection_failure_reports_its_node_after_decoding() {
    let (_directory, mut service, track, _) = fixture();
    let config = graph_config(vec![GraphNodeKind::Denoise {
        config: Default::default(),
        profile: Some(NoiseSelection {
            start_seconds: 0.0,
            end_seconds: 1.0,
        }),
    }]);
    assert_graph_failure(&mut service, &track, config, 3, "outside the source");
}

#[test]
fn native_graph_decode_budget_failure_reports_requested_output_once() {
    let (_directory, mut old, track, _) = fixture();
    old.shutdown();
    let mut service = Service::with_memory_limit(1);
    assert_graph_failure(
        &mut service,
        &track,
        graph_config(Vec::new()),
        2,
        "PCM memory budget exceeded",
    );
    assert_eq!(service.memory_bytes(), 0);
}

#[test]
fn neural_channel_rejection_uses_actual_decoder_not_stale_track_fields() {
    let (directory, mut service, mut track, mut config) = fixture();
    let manifest = directory.path().join("manifest.json");
    fs::write(&manifest, b"{}").unwrap();
    let settings = Arc::make_mut(&mut config);
    settings.neural.speech_manifest = Some(manifest.clone());
    settings.neural.separation_manifest = Some(manifest.clone());
    // Format rejection must precede interpreting either admitted file.
    settings.neural.worker = manifest;
    let mut writer = hound::WavWriter::create(
        &track.path,
        hound::WavSpec {
            channels: 3,
            sample_rate: 8000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for _ in 0..16 {
        for sample in [0.1_f32, 0.2, 0.3] {
            writer.write_sample(sample).unwrap();
        }
    }
    writer.finalize().unwrap();
    track.channels = 2;
    for operation in [ProcessingOperation::Speech, ProcessingOperation::Separate] {
        service
            .start(track.clone(), config.clone(), operation, None)
            .unwrap();
        loop {
            if let Event::Finished { result, .. } = next_event(&mut service) {
                service.finish_task(service.active_id().unwrap()).unwrap();
                let error = format!("{:#}", result.err().unwrap());
                assert!(error.contains("mono/stereo") || error.contains("stereo source"));
                break;
            }
        }
    }
    let error = completed_graph(
        &mut service,
        &track,
        graph_config(vec![GraphNodeKind::Speech {
            config: Default::default(),
        }]),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("unsupported channel layout"));
}

fn block_reclamation(service: &mut Service) -> std::sync::mpsc::Sender<()> {
    for _ in 0..service.sender.capacity().unwrap() {
        service
            .sender
            .try_send(Event::Progress {
                id: 0,
                completed: 0,
                total: None,
                phase: ProcessingPhase::Queued,
            })
            .ok()
            .unwrap();
    }
    service
        .reclaimer
        .as_ref()
        .unwrap()
        .hold(service.sender.clone())
}

fn unblock_reclamation(service: &mut Service, release: std::sync::mpsc::Sender<()>) {
    for _ in 0..service.sender.capacity().unwrap() {
        assert!(matches!(next_event(service), Event::Progress { id: 0, .. }));
    }
    release.send(()).unwrap();
}

#[test]
fn failed_processing_spawns_do_not_consume_retirement_capacity() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let budget = Arc::clone(&service.budget);
        let release = block_reclamation(&mut service);
        service.reclaimer.as_mut().unwrap().fail_spawns = true;
        for _ in 0..64 {
            let error = service
                .start(
                    track.clone(),
                    config.clone(),
                    ProcessingOperation::Denoise,
                    None,
                )
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("injected processing worker spawn failure")
            );
            assert!(service.active_id().is_none());
        }
        assert_eq!(service.sender.len(), service.sender.capacity().unwrap());
        service.reclaimer.as_mut().unwrap().fail_spawns = false;
        let status = service
            .start(track, config, ProcessingOperation::Denoise, None)
            .unwrap();
        unblock_reclamation(&mut service, release);
        loop {
            match next_event(&mut service) {
                Event::Progress { .. } => {}
                Event::Finished { id, result } => {
                    assert_eq!(id, status.id);
                    let result = result.unwrap();
                    assert_eq!(samples(result.outputs.denoised.as_ref().unwrap()), PCM);
                    service.retire(result);
                    break;
                }
                _ => panic!("unexpected processing event after retry"),
            }
        }
        service.shutdown();
        assert!(service.active_id().is_none());
        assert!(budget.used_bytes() > 0);
        drop(service);
        assert_eq!(budget.used_bytes(), 0);
    });
}

#[test]
fn failed_mix_spawns_do_not_consume_retirement_capacity() {
    bounded(|| {
        let (_directory, mut service, _track, _config) = fixture();
        let budget = Arc::clone(&service.budget);
        let release = block_reclamation(&mut service);
        service.reclaimer.as_mut().unwrap().fail_spawns = true;
        for _ in 0..64 {
            let error = service.start_mix(1, 7, 0.0, 0.0).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("injected processing worker spawn failure")
            );
            assert!(service.active_id().is_none());
        }
        assert_eq!(service.sender.len(), service.sender.capacity().unwrap());
        service.reclaimer.as_mut().unwrap().fail_spawns = false;
        service.start_mix(1, 7, 0.0, 0.0).unwrap();
        unblock_reclamation(&mut service, release);
        let path = match next_event(&mut service) {
            Event::MixFinished {
                id,
                generation,
                result,
            } => {
                assert_eq!((id, generation), (1, 7));
                result.unwrap()
            }
            _ => panic!("stem mix did not complete after retry"),
        };
        let actual = samples(&path);
        assert_eq!(actual, PCM.map(|sample| sample * 2.0));
        service.shutdown();
        assert!(budget.used_bytes() > 0);
        drop(service);
        assert!(budget.used_bytes() > 0);
        drop(path);
        assert_eq!(budget.used_bytes(), 0);
    });
}

#[test]
fn shutdown_releases_worker_waiting_behind_reclamation_backpressure() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let budget = Arc::clone(&service.budget);
        let _release = block_reclamation(&mut service);
        service
            .start(
                track.clone(),
                config.clone(),
                ProcessingOperation::Denoise,
                None,
            )
            .unwrap();
        service.shutdown();
        assert!(service.active_id().is_none());
        assert!(service.receiver.is_closed());
        assert!(budget.used_bytes() > 0);
        assert!(track.path.is_file());
        let error = service
            .start(track, config, ProcessingOperation::Denoise, None)
            .unwrap_err();
        assert!(error.to_string().contains("shutting down"));
        assert!(service.start_mix(1, 7, 0.0, 0.0).is_err());
        drop(service);
        assert_eq!(budget.used_bytes(), 0);
    });
}

#[test]
fn retirement_keeps_audition_alias_charged_until_last_owner_releases() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let result = completed(&mut service, &track, &config);
        let alias = result.outputs.denoised.as_ref().unwrap().clone();
        let bytes = alias.retained_bytes();
        let before = service.memory_bytes();
        service.retire(Box::new(result));
        service.shutdown();
        assert_eq!(service.memory_bytes(), before);
        drop(alias);
        assert_eq!(service.memory_bytes(), before - bytes);
    });
}

#[test]
fn cancelled_admitted_worker_releases_its_barrier_and_allows_retry() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let before = service.memory_bytes();
        let release = block_reclamation(&mut service);
        let status = service
            .start(
                track.clone(),
                config.clone(),
                ProcessingOperation::Denoise,
                None,
            )
            .unwrap();
        assert!(service.cancel(status.id + 1).is_err());
        service.cancel(status.id).unwrap();
        unblock_reclamation(&mut service, release);
        loop {
            match next_event(&mut service) {
                Event::Progress { .. } => {}
                Event::Finished { id, result } => {
                    assert_eq!(id, status.id);
                    assert!(format!("{:#}", result.err().unwrap()).contains("cancelled"));
                    service.finish_task(service.active_id().unwrap()).unwrap();
                    break;
                }
                _ => panic!("unexpected event from cancelled processing"),
            }
        }
        assert_eq!(service.memory_bytes(), before);
        let retry = completed(&mut service, &track, &config);
        assert!(retry.status.id > status.id);
        assert_eq!(samples(retry.outputs.denoised.as_ref().unwrap()), PCM);
    });
}

#[test]
fn pcm_budget_rejects_inflight_allocation_without_creating_processing_directory() {
    let (directory, mut old, track, config) = fixture();
    old.shutdown();
    let entries = || {
        std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let before = entries();
    let mut service = Service::with_memory_limit(1);
    service
        .start(track, config, ProcessingOperation::Denoise, None)
        .unwrap();
    loop {
        if let Event::Finished { result, .. } = next_event(&mut service) {
            let error = result.err().unwrap();
            assert!(format!("{error:#}").contains("PCM memory budget exceeded"));
            service.finish_task(service.active_id().unwrap()).unwrap();
            break;
        }
    }
    assert_eq!(service.memory_bytes(), 0);
    assert_eq!(entries(), before);
}

#[test]
fn explicit_wav_export_is_atomic_and_no_clobber() {
    let (directory, service, _, _) = fixture();
    let pcm = service.results()[0].outputs.vocals.as_ref().unwrap();
    let destination = directory.path().join("export.wav");
    let (temporary, canonical) =
        prepare_export(pcm, &destination, &AtomicBool::new(false), |_, _| {}).unwrap();
    assert!(!destination.exists());
    temporary.persist_noclobber(canonical).unwrap();
    let mut reader = hound::WavReader::open(&destination).unwrap();
    assert_eq!(reader.spec().sample_rate, pcm.sample_rate());
    assert_eq!(reader.spec().channels, pcm.channels());
    assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
    let decoded: Vec<f32> = reader
        .samples()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(decoded, PCM);
    assert!(prepare_export(pcm, &destination, &AtomicBool::new(false), |_, _| {}).is_err());
    let cancelled = directory.path().join("cancelled.wav");
    assert!(prepare_export(pcm, &cancelled, &AtomicBool::new(true), |_, _| {}).is_err());
    assert!(!cancelled.exists());
    let before = fs::read_dir(directory.path()).unwrap().count();
    let cancel = AtomicBool::new(false);
    let mut wrote_frames = false;
    let error = prepare_export(pcm, &cancelled, &cancel, |done, _| {
        if done > 0 {
            wrote_frames = true;
            cancel.store(true, Ordering::Release);
        }
    })
    .err()
    .unwrap();
    assert!(wrote_frames);
    assert!(error.to_string().contains("cancelled"));
    assert!(!cancelled.exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), before);

    let raced = directory.path().join("raced.wav");
    let (temporary, canonical) =
        prepare_export(pcm, &raced, &AtomicBool::new(false), |_, _| {}).unwrap();
    fs::write(&raced, b"created after preparation").unwrap();
    let error = temporary.persist_noclobber(canonical).unwrap_err();
    drop(error);
    assert_eq!(fs::read(&raced).unwrap(), b"created after preparation");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), before + 1);
}

#[test]
fn wav_export_preserves_side_speakers_and_reorders_custom_frames() {
    use audio::Channel::*;
    let directory = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    for (name, speakers) in [
        (
            "side",
            vec![FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight],
        ),
        (
            "custom",
            vec![SideRight, FrontCenter, FrontLeft, Lfe, SideLeft, FrontRight],
        ),
        ("left-only", vec![FrontLeft]),
    ] {
        let layout: Arc<[audio::Channel]> = speakers.into();
        let budget = audio::PcmBudget::new(256 * 1024);
        let mut builder =
            audio::PcmBuilder::new(48_000, Arc::clone(&layout), Arc::clone(&budget)).unwrap();
        let input: Vec<_> = (0..3)
            .flat_map(|frame| {
                layout
                    .iter()
                    .map(move |speaker| frame as f32 - speaker.standard_index() as f32 / 8.0)
            })
            .collect();
        builder.push(&input).unwrap();
        let pcm = builder.finish().unwrap();
        let destination = directory.path().join(format!("{name}.wav"));
        let (temporary, canonical) =
            prepare_export(&pcm, &destination, &cancel, |_, _| {}).unwrap();
        temporary.persist_noclobber(canonical).unwrap();
        assert_eq!(pcm.layout().as_ref(), layout.as_ref());
        assert_eq!(samples(&pcm), input);

        let mut canonical_layout = layout.to_vec();
        canonical_layout.sort_unstable_by_key(|speaker| speaker.standard_index());
        let mut expected = Vec::new();
        for frame in input.chunks_exact(layout.len()) {
            for speaker in &canonical_layout {
                expected.push(frame[layout.iter().position(|input| input == speaker).unwrap()]);
            }
        }
        let exported: Vec<f32> = hound::WavReader::open(&destination)
            .unwrap()
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(exported, expected);
        let renderer = audio::SourceRenderer::open(
            &destination,
            None,
            rivu_format::decoder_hint::DecoderHint::Native,
            1,
            false,
            &cancel,
        )
        .unwrap();
        assert_eq!(renderer.format().sample_rate, 48_000);
        assert_eq!(renderer.format().layout.as_ref(), canonical_layout);
        assert_eq!(renderer.format().channels(), pcm.channels());
        let identity = renderer.format().source_identity;
        let decoded = renderer.render_pcm(Arc::clone(&budget), &cancel).unwrap();
        assert_eq!(decoded.layout().as_ref(), canonical_layout);
        assert_eq!(samples(&decoded), expected);
        let restored = audio::SourceRenderer::from_pcm(decoded, identity).unwrap();
        assert_eq!(restored.format().layout.as_ref(), canonical_layout);
    }
}

#[test]
fn unpinned_audio_release_preserves_frozen_history_and_selected_audition() {
    let (_directory, mut service, track, config) = fixture();
    let result = completed(&mut service, &track, &config);
    let _ = service.retain_result(Box::new(result));
    service.select_audition(Some((1, 0)));
    service.release_unpinned_audio();
    assert!(service.results()[0].outputs.vocals.is_some());
    assert!(service.results()[1].outputs.denoised.is_none());
    assert!(service.results()[1].status.outputs.is_none());
    assert_eq!(service.results()[1].track.path, track.path);
    assert_eq!(service.results()[1].status.settings.denoise, config.denoise);
    service.shutdown();
    assert_eq!(
        service.memory_bytes(),
        service.results()[0]
            .outputs
            .vocals
            .as_ref()
            .unwrap()
            .retained_bytes()
    );
}

#[test]
fn realtime_preparation_never_creates_processing_storage() {
    let (directory, mut service, track, config) = fixture();
    let before = service.memory_bytes();
    service
        .start_graph(
            track,
            config,
            GraphRequest {
                target: GraphPort { node: 2, port: 0 },
                mode: GraphExecutionMode::Realtime,
                export_path: None,
                source_run: None,
                cached_output: None,
            },
        )
        .unwrap();
    loop {
        if let Event::Finished { result, .. } = next_event(&mut service) {
            service.finish_task(service.active_id().unwrap()).unwrap();
            let result = result.unwrap();
            assert!(result.runner.is_some());
            assert!(result.graph_outputs.is_empty());
            break;
        }
    }
    assert_eq!(service.memory_bytes(), before);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn rejected_admission_preserves_unselected_pcm_until_successful_spawn() {
    let (_directory, mut service, track, config) = fixture();
    service.select_audition(None);
    let before = service.memory_bytes();
    assert!(
        service
            .start(
                track.clone(),
                config.clone(),
                ProcessingOperation::Denoise,
                Some(NoiseSelection {
                    start_seconds: 1.0,
                    end_seconds: 0.0
                })
            )
            .is_err()
    );
    assert!(service.results()[0].outputs.vocals.is_some());
    assert_eq!(service.memory_bytes(), before);
    service.reclaimer.as_mut().unwrap().fail_spawns = true;
    assert!(
        service
            .start(
                track.clone(),
                config.clone(),
                ProcessingOperation::Denoise,
                None
            )
            .is_err()
    );
    assert!(service.results()[0].outputs.vocals.is_some());
    assert_eq!(service.memory_bytes(), before);
    service.reclaimer.as_mut().unwrap().fail_spawns = false;
    let result = completed(&mut service, &track, &config);
    assert!(service.results()[0].outputs.vocals.is_none());
    assert!(service.results()[0].status.outputs.is_none());
    assert_eq!(
        service.memory_bytes(),
        result.outputs.denoised.as_ref().unwrap().retained_bytes()
    );
}

#[test]
fn native_decoder_streams_fit_one_output_budget_including_profile_passes() {
    let (directory, old, track, _) = fixture();
    drop(old);
    let mut writer = hound::WavWriter::create(
        &track.path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    let data: Vec<f32> = (0..20000)
        .map(|frame| (frame as f32 * 0.07).sin() * 0.2)
        .collect();
    for &sample in &data {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    for selection in [
        None,
        Some(NoiseSelection {
            start_seconds: 0.1,
            end_seconds: 0.3,
        }),
        Some(NoiseSelection {
            start_seconds: 0.1,
            end_seconds: 2.5,
        }),
    ] {
        let mut service = Service::with_memory_limit(100_000);
        let mut config = Config::default();
        if selection.is_some() {
            config.denoise.mode = rivu_dsp::denoise::DenoiseMode::Profile;
            config.denoise.fft_size = 256;
        } else {
            config.denoise.hum.enabled = true;
        }
        let noise = selection.map(|selection| {
            let start = (selection.start_seconds * 8000.0).floor() as usize;
            let end = (selection.end_seconds * 8000.0).floor() as usize;
            let mut collector =
                rivu_dsp::denoise::ProfileCollector::new(8_000, 1, 256, (end - start) as u64)
                    .unwrap();
            collector
                .push_block(&data[start..end], &mut || false)
                .unwrap();
            collector.finish().unwrap()
        });
        let mut reference =
            rivu_dsp::denoise::Denoiser::new(8_000, 1, config.denoise.clone(), noise).unwrap();
        let mut expected = Vec::new();
        reference
            .process_block(&data, &mut expected, &mut || false)
            .unwrap();
        reference.finish(&mut expected, &mut || false).unwrap();
        service
            .start(
                track.clone(),
                Arc::new(config),
                ProcessingOperation::Denoise,
                selection,
            )
            .unwrap();
        loop {
            if let Event::Finished { result, .. } = next_event(&mut service) {
                service.finish_task(service.active_id().unwrap()).unwrap();
                let result = result.unwrap();
                let pcm = result.outputs.denoised.as_ref().unwrap();
                assert_eq!(pcm.frames(), 20000);
                assert_eq!(service.memory_bytes(), pcm.retained_bytes());
                let actual = samples(pcm);
                assert_eq!(actual.len(), expected.len());
                for (actual, &expected) in actual.into_iter().zip(&expected) {
                    assert!((actual - expected).abs() < 1e-5);
                }
                break;
            }
        }
    }
    let mut service = Service::with_memory_limit(100_000);
    let config = graph_config(vec![
        GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                ..Default::default()
            },
            implementation: EqImplementation::LinearPhase { taps: 65 },
        },
        GraphNodeKind::Denoise {
            config: rivu_dsp::denoise::DenoiseConfig {
                mode: rivu_dsp::denoise::DenoiseMode::Profile,
                fft_size: 256,
                ..Default::default()
            },
            profile: Some(NoiseSelection {
                start_seconds: 0.1,
                end_seconds: 0.3,
            }),
        },
    ]);
    let result = completed_graph(&mut service, &track, config).unwrap();
    let output = result
        .graph_outputs
        .get(&GraphPort { node: 2, port: 0 })
        .unwrap();
    assert_eq!(output.pcm.frames(), 20000);
    assert_eq!(service.memory_bytes(), output.pcm.retained_bytes());
    assert!(output.context.is_some());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn guarded_profile_selection_uses_source_frame_floor_in_both_execution_paths() {
    use rivu_dsp::{
        denoise::{DenoiseConfig, DenoiseMode, Denoiser, ProfileCollector},
        equalizer::{config::EqualizerConfig, fir::FirEqualizer},
    };

    let (directory, mut service, track, _) = fixture();
    let mut data: Vec<f32> = (0..1400).map(|i| (i as f32 * 0.07).sin() * 0.3).collect();
    data[26] = 0.9;
    data[27] = -0.7;
    let mut writer = hound::WavWriter::create(
        &track.path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for &sample in &data {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    for selection in [
        NoiseSelection {
            start_seconds: 0.0005625,
            end_seconds: 0.012,
        },
        NoiseSelection {
            start_seconds: 0.018,
            end_seconds: 1400.25 / 48_000.0,
        },
        NoiseSelection {
            start_seconds: 1399.25 / 48_000.0,
            end_seconds: 1400.25 / 48_000.0,
        },
    ] {
        for profile_count in 1..=2 {
            let start = (selection.start_seconds * 48_000.0).floor() as usize;
            let end = (selection.end_seconds * 48_000.0).floor() as usize;
            let eq = EqualizerConfig {
                enabled: true,
                ..Default::default()
            };
            let denoise = DenoiseConfig {
                mode: DenoiseMode::Profile,
                fft_size: 256,
                ..Default::default()
            };
            let mut fir = FirEqualizer::new(&eq, 48_000, 1, 65).unwrap();
            let mut latency = fir.latency_frames();
            if selection.start_seconds == 0.0005625 {
                // Floor source time before adding guards, not a rounded sum of seconds.
                assert_ne!(
                    ((selection.start_seconds + latency as f64 / 48_000.0) * 48_000.0).floor()
                        as usize,
                    start + latency
                );
            }
            let mut upstream = data.clone();
            upstream.resize(data.len() + fir.tail_frames(), 0.0);
            fir.process_interleaved(&mut upstream).unwrap();
            let mut kinds = vec![GraphNodeKind::Equalizer {
                config: eq,
                implementation: EqImplementation::LinearPhase { taps: 65 },
            }];
            for _ in 0..profile_count {
                // Build the reference from the complete upstream output, including
                // true EOF context. Prefix capture must produce the identical profile.
                let mut collector =
                    ProfileCollector::new(48_000, 1, 256, (end - start) as u64).unwrap();
                collector
                    .push_block(&upstream[start + latency..end + latency], &mut || false)
                    .unwrap();
                let mut denoiser = Denoiser::new(
                    48_000,
                    1,
                    denoise.clone(),
                    Some(collector.finish().unwrap()),
                )
                .unwrap();
                let lookahead = denoiser.lookahead_frames();
                let mut processed = Vec::new();
                denoiser
                    .process_block(&upstream, &mut processed, &mut || false)
                    .unwrap();
                denoiser.finish(&mut processed, &mut || false).unwrap();
                upstream = vec![0.0; lookahead];
                upstream.extend(processed);
                latency += lookahead;
                kinds.push(GraphNodeKind::Denoise {
                    config: denoise.clone(),
                    profile: Some(selection),
                });
            }
            let expected = &upstream[latency..latency + data.len()];
            let result = completed_graph(&mut service, &track, graph_config(kinds)).unwrap();
            let output = &result.graph_outputs[&GraphPort { node: 2, port: 0 }];
            let decoded = samples(&output.pcm);
            assert_eq!(decoded.len(), expected.len());
            for (&actual, &expected) in decoded.iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-5);
            }
            let mut builder = audio::PcmBuilder::new(
                48_000,
                Arc::from([audio::Channel::FrontCenter]),
                Arc::clone(&service.budget),
            )
            .unwrap();
            builder.push(&data).unwrap();
            let input = builder.finish().unwrap();
            let (tx, _rx) = async_channel::unbounded();
            let mut progress = ProgressReporter::new(&tx, result.status.id);
            let retained = graph::execute(
                &input,
                Arc::clone(&service.budget),
                &mut result.status.clone(),
                &AtomicBool::new(false),
                &mut progress,
            )
            .unwrap();
            let retained = samples(&retained[&GraphPort { node: 2, port: 0 }].pcm);
            assert_eq!(retained.len(), expected.len());
            for (actual, &expected) in retained.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-5);
            }
        }
    }
    // FIR tail can supply output after the source ends; it must never make an
    // out-of-source selection look valid merely because its profile was filled.
    assert_graph_failure(
        &mut service,
        &track,
        graph_config(vec![
            GraphNodeKind::Equalizer {
                config: EqualizerConfig {
                    enabled: true,
                    ..Default::default()
                },
                implementation: EqImplementation::LinearPhase { taps: 65 },
            },
            GraphNodeKind::Denoise {
                config: DenoiseConfig {
                    mode: DenoiseMode::Profile,
                    fft_size: 256,
                    ..Default::default()
                },
                profile: Some(NoiseSelection {
                    start_seconds: 0.0,
                    end_seconds: 1401.25 / 48_000.0,
                }),
            },
        ]),
        4,
        "outside the source",
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}
