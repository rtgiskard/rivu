use super::*;

const PCM: [f32; 4] = [0.125, -0.25, 0.5, 0.0];
const BLOCKED_PUBLICATION: u64 = u64::MAX;

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
                service.active.take().unwrap().worker.join().unwrap();
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
    let mut service = Service::new(directory.path().join("processing"));
    let mut result = completed(&mut service, &track, &config);
    let stem = result
        .status
        .outputs
        .as_ref()
        .unwrap()
        .denoised
        .clone()
        .unwrap();
    result.status.outputs = Some(ProcessingOutputs {
        vocals: Some(stem.clone()),
        accompaniment: Some(stem),
        ..Default::default()
    });
    service.results.push_back(result);
    (directory, service, track, config)
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
            cached_output: None,
        },
    )?;
    loop {
        match next_event(service) {
            Event::Progress { .. } | Event::GraphNode { .. } => {}
            Event::Finished { result, .. } => {
                service.active.take().unwrap().worker.join().unwrap();
                return result.map(|result| *result);
            }
            _ => panic!("unexpected graph event"),
        }
    }
}

#[test]
fn offline_bypass_preserves_shared_causal_pcm_and_publication() {
    let (_directory, mut service, track, _) = fixture();
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
    let output = |node| {
        result
            .status
            .graph_outputs
            .iter()
            .find(|output| output.port.node == node)
            .unwrap()
    };
    let processed = output(3);
    let context = processed.context.as_ref().unwrap();
    assert!(context.start_frame < 0);
    assert_eq!(
        hound::WavReader::open(&processed.path).unwrap().duration(),
        PCM.len() as u32
    );
    for node in [4, 5, 2] {
        assert_eq!(output(node).path, processed.path);
        assert_eq!(output(node).context.as_ref().unwrap().path, context.path);
        assert_eq!(
            output(node).context.as_ref().unwrap().start_frame,
            context.start_frame
        );
    }
    assert!(!result.directory.path().join("node-4").exists());
    assert!(!result.directory.path().join("node-5").exists());
}

#[test]
fn offline_off_denoise_keeps_hum_and_impulse_active_but_wet_zero_shares_input() {
    for mode in 0..3 {
        let (_directory, mut service, track, _) = fixture();
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
                config: denoise,
                profile: None,
            }]),
        )
        .unwrap();
        let source = result
            .status
            .graph_outputs
            .iter()
            .find(|output| output.port.node == 1)
            .unwrap();
        let denoised = result
            .status
            .graph_outputs
            .iter()
            .find(|output| output.port.node == 3)
            .unwrap();
        assert_eq!(denoised.path == source.path, mode == 2);
        assert_eq!(result.directory.path().join("node-3").exists(), mode != 2);
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
    let error = completed_graph(&mut service, &track, config).err().unwrap();
    assert!(format!("{error:#}").contains("outside the source"));
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
                service.active.take().unwrap().worker.join().unwrap();
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
    assert!(format!("{error:#}").contains("source channel format is unsupported"));
}

fn block_reclamation(service: &mut Service, track: &Track, config: &Arc<Config>) {
    let result = completed(service, track, config);
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
    // Publication cannot pass the full reliable event channel. Every barrier
    // queued afterward stays behind it, independent of worker scheduling.
    service.publish_after_retirement(BLOCKED_PUBLICATION, Box::new(result));
}

fn unblock_reclamation(service: &mut Service) {
    for _ in 0..service.sender.capacity().unwrap() {
        assert!(matches!(next_event(service), Event::Progress { id: 0, .. }));
    }
    match next_event(service) {
        Event::ReadyToPublish { id, result } => {
            assert_eq!(id, BLOCKED_PUBLICATION);
            service.retire(result);
        }
        _ => panic!("reclaimer did not deliver the blocked publication"),
    }
}

#[test]
fn failed_processing_spawns_do_not_consume_retirement_capacity() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let retained = service.results[0].directory.path().to_path_buf();
        block_reclamation(&mut service, &track, &config);
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
            assert!(service.active.is_none());
        }
        assert_eq!(service.sender.len(), service.sender.capacity().unwrap());
        assert_eq!(service.retiring_bytes(), 0);
        service.reclaimer.as_mut().unwrap().fail_spawns = false;
        let status = service
            .start(track, config, ProcessingOperation::Denoise, None)
            .unwrap();
        unblock_reclamation(&mut service);
        loop {
            match next_event(&mut service) {
                Event::Progress { .. } => {}
                Event::Finished { id, result } => {
                    assert_eq!(id, status.id);
                    let result = result.unwrap();
                    assert!(
                        result
                            .status
                            .outputs
                            .as_ref()
                            .unwrap()
                            .denoised
                            .as_ref()
                            .unwrap()
                            .is_file()
                    );
                    service.retire(result);
                    break;
                }
                _ => panic!("unexpected processing event after retry"),
            }
        }
        service.shutdown();
        assert!(service.active.is_none());
        assert_eq!(service.retiring_bytes(), 0);
        assert!(retained.is_dir());
        drop(service);
        assert!(!retained.exists());
    });
}

#[test]
fn failed_mix_spawns_do_not_consume_retirement_capacity() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let retained = service.results[0].directory.path().to_path_buf();
        block_reclamation(&mut service, &track, &config);
        service.reclaimer.as_mut().unwrap().fail_spawns = true;
        for _ in 0..64 {
            let error = mix(&mut service, 1, 7, 0.0, 0.0).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("injected processing worker spawn failure")
            );
            assert!(service.active.is_none());
        }
        assert_eq!(service.sender.len(), service.sender.capacity().unwrap());
        assert_eq!(service.retiring_bytes(), 0);
        service.reclaimer.as_mut().unwrap().fail_spawns = false;
        mix(&mut service, 1, 7, 0.0, 0.0).unwrap();
        unblock_reclamation(&mut service);
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
        let actual = hound::WavReader::open(&path)
            .unwrap()
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(actual, PCM.map(|sample| sample * 2.0));
        service.shutdown();
        assert_eq!(service.retiring_bytes(), 0);
        assert!(path.is_file());
        assert!(retained.is_dir());
        drop(service);
        assert!(!retained.exists());
    });
}

#[test]
fn shutdown_releases_worker_waiting_behind_reclamation_backpressure() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let retained = service.results[0].directory.path().to_path_buf();
        block_reclamation(&mut service, &track, &config);
        service
            .start(
                track.clone(),
                config.clone(),
                ProcessingOperation::Denoise,
                None,
            )
            .unwrap();
        service.shutdown();
        assert!(service.active.is_none());
        assert_eq!(service.retiring_bytes(), 0);
        assert!(service.receiver.is_closed());
        assert!(retained.is_dir());
        assert!(track.path.is_file());
        let error = service
            .start(track, config, ProcessingOperation::Denoise, None)
            .unwrap_err();
        assert!(error.to_string().contains("shutting down"));
        assert!(mix(&mut service, 1, 7, 0.0, 0.0).is_err());
        drop(service);
        assert!(!retained.exists());
    });
}

#[test]
fn reclamation_failure_rejects_admitted_worker_and_preserves_charged_bytes() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let result = completed(&mut service, &track, &config);
        let bytes = result.cache_bytes;
        assert!(bytes > 0);
        let retired = result.directory.path().to_path_buf();
        fs::remove_dir_all(&retired).unwrap();
        fs::write(&retired, b"not a directory").unwrap();
        service.retire(Box::new(result));
        let status = service
            .start(track.clone(), config, ProcessingOperation::Denoise, None)
            .unwrap();
        match next_event(&mut service) {
            Event::Finished { id, result } => {
                assert_eq!(id, status.id);
                let Err(error) = result else {
                    panic!("reclamation failure was ignored");
                };
                assert!(
                    error
                        .to_string()
                        .contains("Processing cache reclamation failed")
                );
            }
            _ => panic!("failed reclamation allowed processing to begin"),
        }
        assert_eq!(service.retiring_bytes(), bytes);
        assert!(track.path.is_file());
        service.shutdown();
        assert!(service.results[0].directory.path().is_dir());
    });
}
