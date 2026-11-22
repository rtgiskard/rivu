use super::export::prepare_export;
use super::render::ProgressReporter;
use super::*;
use rivu_audio::test_support::write_float_wav;
use std::time::Duration;

pub(super) fn samples(pcm: &audio::PcmAudio) -> Vec<f32> {
    let mut samples = vec![0.0; pcm.frames() as usize * usize::from(pcm.channels())];
    assert_eq!(pcm.read_frames(0, &mut samples), pcm.frames() as usize);
    samples
}

const PCM: [f32; 4] = [0.125, -0.25, 0.5, 0.0];

fn bounded(run: impl FnOnce()) {
    if !rivu_utils::test_support::run_in_subprocess(Duration::from_secs(10)) {
        run();
    }
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
                assert_eq!(id, status);
                service.finish_task(service.active_id().unwrap()).unwrap();
                return *result.unwrap();
            }
            _ => panic!("unexpected processing event"),
        }
    }
}

fn source_fixture(
    channels: u16,
    sample_rate: u32,
    samples: impl IntoIterator<Item = f32>,
) -> (tempfile::TempDir, Track, Arc<Config>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.wav");
    write_float_wav(&path, channels, sample_rate, samples).unwrap();
    let mut track = rivu_protocol::test_support::artwork_track(path);
    track.channels = channels;
    track.sample_rate = sample_rate;
    track.bits_per_sample = Some(32);
    let config = Arc::new(Config::default());
    (directory, track, config)
}

fn fixture() -> (tempfile::TempDir, Service, Track, Arc<Config>) {
    let (directory, track, config) = source_fixture(1, 8_000, PCM);
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
    service.accept_result(Box::new(result));
    let pcm = service.results()[0]
        .outputs
        .vocals
        .as_ref()
        .unwrap()
        .clone();
    service.select_cached_audition(1, 0, pcm, None);
    (directory, service, track, config)
}

#[test]
fn aborted_task_terminalizes_unfinished_nodes_preserving_specific_results() {
    let (_directory, mut service, _track, _config) = fixture();
    let id = service.results[0].id;
    for phase in [ProcessingPhase::Failed, ProcessingPhase::Cancelled] {
        for already_terminal in [false, true] {
            let status = Arc::make_mut(service.records.get_mut(&id).unwrap());
            let error = Some("task stopped".to_owned());
            status.phase = if already_terminal {
                phase
            } else {
                ProcessingPhase::Processing
            };
            status.error = already_terminal.then(|| "task stopped".into());
            status.graph_nodes = [
                ProcessingPhase::Complete,
                ProcessingPhase::Failed,
                ProcessingPhase::Processing,
                ProcessingPhase::Queued,
                ProcessingPhase::Rendering,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, phase)| GraphNodeStatus {
                node: index as u64,
                phase,
                error: (phase == ProcessingPhase::Failed).then(|| "specific node failure".into()),
                model_id: None,
                model_sha256: None,
            })
            .collect();

            service.outcome(id, phase, error.clone());

            let status = service.status(id).unwrap();
            assert_eq!(status.phase, phase);
            assert_eq!(status.error, error);
            assert_eq!(status.graph_nodes[0].phase, ProcessingPhase::Complete);
            assert_eq!(status.graph_nodes[1].phase, ProcessingPhase::Failed);
            assert_eq!(
                status.graph_nodes[1].error.as_deref(),
                Some("specific node failure")
            );
            for node in &status.graph_nodes[2..] {
                assert_eq!(node.phase, phase);
                assert!(node.error.is_none());
            }
        }
    }
}

#[test]
fn cached_export_rejects_source_replacement_between_lookup_and_task_admission() {
    let (directory, mut service, track, config) = fixture();
    let result = &service.results[0];
    let cached = CachedGraphOutput {
        pcm: result.outputs.vocals.as_ref().unwrap().clone(),
        stamp: result.stamp,
    };
    // Deterministically split Core's retained-result check from Service admission.
    assert_eq!(SourceStamp::read(&track.path).unwrap(), cached.stamp);
    fs::write(&track.path, b"replacement after retained-result lookup").unwrap();
    let destination = directory.path().join("stale.wav");
    let next_id = service.next_id;
    let history = service.history().len();
    let bytes = service.memory_bytes();
    let error = service
        .start_graph(
            track,
            config,
            GraphRequest {
                target: GraphPort { node: 2, port: 0 },
                mode: GraphExecutionMode::Offline,
                export_path: Some(destination.clone()),
                source_run: Some(1),
                cached_output: Some(cached),
                audio_export: None,
            },
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("source file changed before export admission")
    );
    assert!(service.active_id().is_none());
    assert_eq!(service.next_id, next_id);
    assert_eq!(service.history().len(), history);
    assert_eq!(service.memory_bytes(), bytes);
    assert!(!destination.exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
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
    let blocked = completed_graph(&mut service, &track, graph_config(vec![])).unwrap();
    let id = blocked.status.id;
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
                id,
                completed: 0,
                total: None,
                phase: ProcessingPhase::Processing,
            })
            .ok()
            .unwrap();
    }
    let stamp = blocked.stamp;
    let cancelled = Arc::new(Cancellation::new().unwrap());
    let observed = Arc::clone(&cancelled);
    let sender = service.sender.clone();
    let (sent, delivery) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let undelivered = deliver_finished(
            &sender,
            Event::Finished {
                id,
                result: Ok(Box::new(blocked)),
            },
        );
        sent.send(undelivered.is_some()).unwrap();
        released.recv().unwrap();
        undelivered
    });
    service.active = Some(ActiveTask {
        id,
        stamp,
        cancelled,
        worker,
    });
    let (finished, completion) = std::sync::mpsc::channel();
    let shutdown = thread::spawn(move || {
        service.shutdown();
        assert!(service.sender.is_closed());
        finished.send(()).unwrap();
        assert!(service.receiver.try_recv().is_err());
        assert_eq!(service.memory_bytes(), retained_bytes);
        let status = service.status(id).unwrap();
        assert_eq!(status.phase, ProcessingPhase::Cancelled);
        assert_eq!(status.frames, Some(PCM.len() as u64));
        assert_eq!(
            status.error.as_deref(),
            Some("Graph output #2:0 was not published: Processing cancelled during shutdown")
        );
        assert_eq!(status.graph_nodes.len(), 2);
        assert!(
            status
                .graph_nodes
                .iter()
                .all(|node| node.phase == ProcessingPhase::Complete)
        );
        assert!(status.graph_outputs.is_empty());
        assert_eq!(service.results().len(), 1);
        service
    });
    let rejected = delivery.recv_timeout(Duration::from_secs(5)).unwrap();
    let waiting_for_retirement = completion.try_recv().is_err();
    let cancelled = observed.flag().load(Ordering::Acquire);
    release.send(()).unwrap();
    completion.recv_timeout(Duration::from_secs(5)).unwrap();
    let service = shutdown.join().unwrap();
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
fn shutdown_keeps_prepared_hybrid_nodes_when_the_offline_prefix_is_cancelled() {
    bounded(|| {
        let (_directory, track, _) = source_fixture(1, 8_000, PCM);
        let mut service = Service::new();
        (service.sender, service.receiver) = async_channel::bounded(1);
        let target = GraphPort { node: 2, port: 0 };
        let boundary = GraphPort { node: 1, port: 0 };
        let id = service
            .start_graph(
                track,
                graph_config(vec![]),
                GraphRequest {
                    target,
                    mode: GraphExecutionMode::Hybrid,
                    export_path: None,
                    source_run: None,
                    cached_output: None,
                    audio_export: None,
                },
            )
            .unwrap();
        // Observe preparation without applying it to the projection. The
        // one-slot queue cannot accept both of the next two stage events.
        assert!(matches!(
            next_event(&mut service),
            Event::GraphPrepared { boundary: Some(port), .. } if port == boundary
        ));
        service.shutdown();

        let status = service.status(id).unwrap();
        assert_eq!(status.phase, ProcessingPhase::Cancelled);
        assert_eq!(status.settings.graph_mode, Some(GraphExecutionMode::Hybrid));
        assert_eq!(status.settings.graph_boundary, Some(boundary));
        assert!(status.graph_plan.is_some());
        assert_eq!(status.graph_nodes[0].phase, ProcessingPhase::Cancelled);
        assert_eq!(status.graph_nodes[1].phase, ProcessingPhase::Complete);
        assert!(status.graph_outputs.is_empty());
        assert!(service.results().is_empty());
        assert_eq!(service.memory_bytes(), 0);
    });
}

#[test]
fn standalone_profile_rejects_empty_frame_and_out_of_source_selections() {
    let (_directory, track, _) = source_fixture(1, 8_000, PCM);
    let mut service = Service::new();
    // Wet zero bypasses denoising but must still validate the profile frames.
    for wet in [1.0, 0.0] {
        let mut config = Config::default();
        config.denoise.mode = rivu_dsp::denoise::DenoiseMode::Profile;
        config.denoise.fft_size = 256;
        config.denoise.wet = wet;
        let config = Arc::new(config);
        for (start, end) in [(0.0, 0.5), (0.0, 5.0), (5.0, 6.0)] {
            service
                .start(
                    track.clone(),
                    Arc::clone(&config),
                    ProcessingOperation::Denoise,
                    Some(NoiseSelection {
                        start_seconds: start / 8_000.0,
                        end_seconds: end / 8_000.0,
                    }),
                )
                .unwrap();
            loop {
                match next_event(&mut service) {
                    Event::Progress { .. } => {}
                    Event::Finished { result, .. } => {
                        service.finish_task(service.active_id().unwrap()).unwrap();
                        assert!(result.is_err(), "wet {wet}, {start}..{end}");
                        break;
                    }
                    _ => panic!("standalone denoise leaked graph events"),
                }
            }
        }
    }
}

#[test]
fn standalone_native_tail_preserves_operation_outputs_and_single_pcm_budget() {
    for frames in [1, 63, 256, 257, 4093] {
        let (_directory, track, _) = source_fixture(
            1,
            8_000,
            (0..frames).map(|frame| (frame as f32 * 0.07).sin() * 0.2),
        );
        let mut service = Service::with_memory_limit(100_000);
        let data: Vec<f32> = (0..frames)
            .map(|frame| (frame as f32 * 0.07).sin() * 0.2)
            .collect();
        let mut config = Config::default();
        config.denoise.mode = rivu_dsp::denoise::DenoiseMode::Stationary;
        config.denoise.fft_size = 256;
        let mut reference =
            rivu_dsp::denoise::Denoiser::new(8_000, 1, config.denoise.clone(), None).unwrap();
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
                None,
            )
            .unwrap();
        let mut source_progress = false;
        let result = loop {
            match next_event(&mut service) {
                Event::Progress {
                    phase: ProcessingPhase::Processing,
                    completed,
                    total,
                    ..
                } => {
                    assert!(completed >= (frames as u64).min(256));
                    assert!(completed <= frames as u64);
                    assert_eq!(total, Some(frames as u64));
                    source_progress = true;
                }
                Event::Progress { .. } => {}
                Event::Finished { result, .. } => {
                    service.finish_task(service.active_id().unwrap()).unwrap();
                    break result.unwrap();
                }
                _ => panic!("standalone denoise leaked graph events"),
            }
        };
        assert!(source_progress);
        assert_eq!(result.status.operation, ProcessingOperation::Denoise);
        assert!(result.status.graph_nodes.is_empty());
        assert!(result.status.graph_outputs.is_empty());
        assert!(result.graph_outputs.is_empty());
        assert!(result.status.outputs.as_ref().unwrap().denoised);
        let pcm = result.outputs.denoised.as_ref().unwrap();
        assert_eq!(pcm.frames(), frames as u64);
        assert_eq!(service.memory_bytes(), pcm.retained_bytes());
        for (actual, expected) in samples(pcm).into_iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-5);
        }
    }
}

pub(super) fn chain(kinds: Vec<GraphNodeKind>) -> ProcessingGraph {
    let mut graph = ProcessingGraph::default();
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
    graph
}

fn graph_config(kinds: Vec<GraphNodeKind>) -> Arc<Config> {
    Arc::new(Config {
        processing_graph: chain(kinds),
        ..Default::default()
    })
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
            audio_export: None,
        },
    )?;
    loop {
        match next_event(service) {
            Event::Progress { .. } | Event::GraphNode { .. } | Event::GraphPrepared { .. } => {}
            Event::Finished { result, .. } => {
                service.finish_task(service.active_id().unwrap()).unwrap();
                return result.map(|result| *result);
            }
            _ => panic!("unexpected graph event"),
        }
    }
}

#[test]
fn graph_models_retain_only_remaining_target_stems_with_bounded_pcm() {
    let (directory, track, _) =
        source_fixture(2, 48_000, (0..4096 * 2).map(|i| PCM[i % PCM.len()]));
    let data: Vec<f32> = (0..4096 * 2).map(|i| PCM[i % PCM.len()]).collect();
    let probe_budget = audio::PcmBudget::new(MEMORY_LIMIT);
    let mut probe = audio::PcmBuilder::new(
        48_000,
        Arc::from([audio::Channel::FrontLeft, audio::Channel::FrontRight]),
        probe_budget,
    )
    .unwrap();
    probe.push(&data).unwrap();
    let source_bytes = probe.finish().unwrap().retained_bytes();
    let manifest = directory.path().join("retention.json");
    fs::write(
        &manifest,
        br#"{"id":"retention-fixture","sha256":"retention-hash"}"#,
    )
    .unwrap();
    let worker = directory.path().join("retention.py");
    fs::write(
        &worker,
        r#"import json, signal, struct, sys
signal.alarm(10)
def read():
    kind, size = struct.unpack('<BI', sys.stdin.buffer.read(5))
    payload = sys.stdin.buffer.read(size)
    assert len(payload) == size
    return kind, payload
def send(kind, payload):
    sys.stdout.buffer.write(struct.pack('<BI', kind, len(payload)) + payload)
kind, payload = read()
assert kind == 0
request = json.loads(payload)
speech = request['operation'] == 'speech'
while True:
    kind, payload = read()
    if kind == 2:
        break
    assert kind == 1
    if speech:
        send(17, payload)
    else:
        values = struct.unpack('<%df' % (len(payload) // 4), payload)
        for stream, gain in [(19, -0.5), (18, 0.25)]:
            send(stream, struct.pack('<%df' % len(values), *(gain * x for x in values)))
    sys.stdout.buffer.flush()
assert sys.stdin.buffer.read() == b''
send(0, json.dumps(dict(version=request['version'], task_id=request['task_id'], type='complete',
    sample_rate=request['sample_rate'], channels=request['channels'], frames=request['frames'],
    outputs=dict(speech=True) if speech else dict(vocals=True, accompaniment=True),
    model=dict(id='retention-fixture', sha256='retention-hash'))).encode())
sys.stdout.buffer.flush()
"#,
    )
    .unwrap();
    let neural = rivu_format::config::NeuralConfig {
        speech_interpreter: "python3".into(),
        separation_interpreter: "python3".into(),
        worker,
        speech_manifest: Some(manifest.clone()),
        separation_manifest: Some(manifest),
        ..Default::default()
    };
    for case in [
        "standalone",
        "vocals",
        "accompaniment",
        "speech-chain",
        "fanout",
        "speech-fanout",
    ] {
        let fanout = case.ends_with("fanout");
        let speech = case.starts_with("speech-");
        let mut config = graph_config(vec![GraphNodeKind::Separate {
            config: neural.clone(),
        }]);
        if case == "standalone" {
            Arc::get_mut(&mut config).unwrap().neural = neural.clone();
            let mut service = Service::with_memory_limit(source_bytes * 3);
            let id = service
                .start(track.clone(), config, ProcessingOperation::Separate, None)
                .unwrap();
            let result = loop {
                match next_event(&mut service) {
                    Event::Progress { .. } | Event::Model { .. } => {}
                    Event::Finished { result, .. } => {
                        service.finish_task(id).unwrap();
                        break result.unwrap();
                    }
                    _ => panic!("unexpected standalone separation event"),
                }
            };
            for (pcm, gain) in [
                (result.outputs.vocals.as_ref().unwrap(), 0.25),
                (result.outputs.accompaniment.as_ref().unwrap(), -0.5),
            ] {
                assert_eq!(
                    samples(pcm),
                    data.iter().map(|sample| sample * gain).collect::<Vec<_>>()
                );
            }
            assert!(result.outputs.denoised.is_none());
            assert_eq!(service.memory_bytes(), source_bytes * 2);
            assert_eq!(result.status.model_id.as_deref(), Some("retention-fixture"));
            assert_eq!(
                result.status.model_sha256.as_deref(),
                Some("retention-hash")
            );
            continue;
        }
        let graph = &mut Arc::get_mut(&mut config).unwrap().processing_graph;
        graph.edges.truncate(1);
        let mut first = GraphPort {
            node: 3,
            port: u16::from(case == "accompaniment"),
        };
        if speech {
            graph.nodes.push(GraphNode {
                id: 4,
                kind: GraphNodeKind::Speech {
                    config: neural.clone(),
                },
                position: [0.0; 2],
            });
            graph.edges.push(GraphEdge {
                from: first,
                to: GraphPort { node: 4, port: 0 },
            });
            first = GraphPort { node: 4, port: 0 };
        }
        if fanout {
            graph.nodes.push(GraphNode {
                id: 5,
                kind: GraphNodeKind::Mix {
                    gains_db: vec![0.0; 2],
                    muted: vec![false; 2],
                    solo: vec![false; 2],
                },
                position: [0.0; 2],
            });
            graph.edges.push(GraphEdge {
                from: first,
                to: GraphPort { node: 5, port: 0 },
            });
            graph.edges.push(GraphEdge {
                from: GraphPort { node: 3, port: 1 },
                to: GraphPort { node: 5, port: 1 },
            });
            first = GraphPort { node: 5, port: 0 };
        }
        graph.edges.push(GraphEdge {
            from: first,
            to: GraphPort { node: 2, port: 0 },
        });
        graph.next_id = 6;
        // Single-stem runs cannot fit source + both stems, even transiently.
        let mut service = Service::with_memory_limit(source_bytes * if fanout { 3 } else { 2 });
        let result = completed_graph(&mut service, &track, config).unwrap();
        let pcm = &result.graph_outputs[&GraphPort { node: 2, port: 0 }].pcm;
        let gain = if fanout {
            -0.25
        } else if case == "accompaniment" {
            -0.5
        } else {
            0.25
        };
        assert_eq!(
            samples(pcm),
            data.iter().map(|sample| sample * gain).collect::<Vec<_>>(),
            "{case}"
        );
        assert_eq!(service.memory_bytes(), source_bytes, "{case}");
        for node in result
            .status
            .graph_nodes
            .iter()
            .filter(|node| node.node == 3 || (speech && node.node == 4))
        {
            assert_eq!(node.model_id.as_deref(), Some("retention-fixture"));
            assert_eq!(node.model_sha256.as_deref(), Some("retention-hash"));
        }
    }
}

pub(super) fn real_dry_speech_bundle(
    directory: &Path,
) -> (rivu_format::config::NeuralConfig, String) {
    // Use the provisioned worker and its runtime sidecars without requiring model inference.
    let setup = std::process::Command::new("python3")
        .arg("-B")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/audio_processing/bundle_fixture.py"),
        )
        .arg(directory)
        .output()
        .expect("dry worker fixture requires python3 on PATH");
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let stdout = String::from_utf8(setup.stdout).unwrap();
    let (hash, interpreter) = stdout.trim_end().split_once('\n').unwrap();
    let config = rivu_format::config::NeuralConfig {
        speech_interpreter: interpreter.into(),
        worker: directory.join("runtime/worker.py"),
        speech_manifest: Some(directory.join("manifest.json")),
        speech_wet_mix: 0.0,
        ..Default::default()
    };
    (config, hash.to_owned())
}

#[test]
fn model_final_export_streams_native_tail_without_a_second_pcm() {
    use rivu_dsp::equalizer::config::{EqBand, EqualizerConfig, FilterKind};

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(30)) {
        return;
    }
    let frames = 4093;
    let (directory, track, _) = source_fixture(
        2,
        48_000,
        (0..frames * 2).map(|i| (i as f32 * 0.17).cos() * 0.3),
    );
    let (model, hash) = real_dry_speech_bundle(directory.path());
    let config = graph_config(vec![
        GraphNodeKind::Speech { config: model },
        GraphNodeKind::Equalizer {
            config: EqualizerConfig {
                enabled: true,
                bands: vec![EqBand {
                    kind: FilterKind::LowPass,
                    frequency_hz: 1500.0,
                    ..Default::default()
                }],
                ..Default::default()
            },
            implementation: EqImplementation::LinearPhase { taps: 257 },
        },
    ]);
    let target = GraphPort { node: 2, port: 0 };
    let destination = directory.path().join("export.wav");
    let request = AudioExportRequest {
        source: AudioExportSource::Graph {
            track_id: track.id,
            target,
            revision: config.processing_graph.revision,
        },
        path: destination.clone(),
        encoding: AudioExportEncoding {
            pcm_bits: 16,
            ..Default::default()
        },
        monitor_eq: None,
    };

    // The dry model returns its validated input. A budget the size of the final causal
    // PCM admits that boundary, but cannot hold both it and a materialized FIR result.
    let reference = completed_graph(&mut Service::new(), &track, Arc::clone(&config)).unwrap();
    let output = &reference.graph_outputs[&target];
    let context = output.context.as_ref().unwrap();
    assert!(context.start_frame < 0 && context.pcm.frames() > frames);
    let mut service = Service::with_memory_limit(context.pcm.retained_bytes());
    let (expected, _) = export::prepare_encoded_export(
        &output.pcm,
        &destination,
        Some(&request),
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();
    let id = service
        .start_graph(
            track,
            config,
            GraphRequest {
                target,
                mode: GraphExecutionMode::Offline,
                export_path: Some(destination.clone()),
                source_run: None,
                cached_output: None,
                audio_export: Some(request),
            },
        )
        .unwrap();
    let result = loop {
        if let Event::Finished { result, .. } = next_event(&mut service) {
            assert_eq!(service.finish_task(id), Some(false));
            break result.unwrap();
        }
    };
    assert!(result.graph_outputs.is_empty() && result.status.graph_outputs.is_empty());
    assert_eq!(result.status.frames, Some(frames));
    assert_eq!(service.memory_bytes(), 0);
    assert!(
        result
            .status
            .graph_nodes
            .iter()
            .all(|n| n.phase == ProcessingPhase::Complete)
    );
    let speech = result
        .status
        .graph_nodes
        .iter()
        .find(|n| n.node == 3)
        .unwrap();
    assert_eq!(speech.model_sha256.as_deref(), Some(hash.as_str()));
    let (temporary, path) = result.pending_export.as_ref().unwrap();
    assert_eq!(path, &destination);
    assert!(!destination.exists());
    let scratch = temporary.path().to_owned();
    assert_eq!(
        fs::read(&scratch).unwrap(),
        fs::read(expected.path()).unwrap()
    );
    drop(result);
    assert!(!scratch.exists() && !destination.exists());
}

#[cfg(feature = "ffmpeg")]
#[test]
fn model_final_export_flush_failure_preserves_completed_nodes() {
    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(30)) {
        return;
    }
    if audio::ExportConverter::new(48_000, 3, 44_100, 3).is_err() {
        assert!(std::env::var_os("RIVU_TEST_REQUIRE_FFMPEG_EXPORT").is_none());
        return;
    }
    // The converter holds the only sample until finish().
    check_model_export_clipping(Some(44_100), 1, true);
}

#[test]
fn model_final_export_direct_write_failure_preserves_only_rendered_nodes() {
    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(30)) {
        return;
    }
    check_model_export_clipping(None, 1, true);
    check_model_export_clipping(None, 4093, false);
}

fn check_model_export_clipping(sample_rate: Option<u32>, frames: usize, fully_rendered: bool) {
    let (directory, track, _) = source_fixture(2, 48_000, std::iter::repeat_n(0.75, frames * 2));
    let (model, _) = real_dry_speech_bundle(directory.path());
    let config = graph_config(vec![
        GraphNodeKind::Speech { config: model },
        GraphNodeKind::Equalizer {
            config: rivu_dsp::equalizer::config::EqualizerConfig {
                enabled: true,
                output_gain_db: 6.0,
                ..Default::default()
            },
            implementation: EqImplementation::Iir,
        },
    ]);
    let output_dir = directory.path().join("exports");
    fs::create_dir(&output_dir).unwrap();
    let path = output_dir.join("output.wav");
    let target = GraphPort { node: 2, port: 0 };
    let request = AudioExportRequest {
        source: AudioExportSource::Graph {
            track_id: track.id,
            target,
            revision: config.processing_graph.revision,
        },
        path: path.clone(),
        encoding: AudioExportEncoding {
            sample_rate,
            pcm_bits: 16,
            ..Default::default()
        },
        monitor_eq: None,
    };
    let mut service = Service::new();
    let task = service
        .start_graph(
            track,
            config,
            GraphRequest {
                target,
                mode: GraphExecutionMode::Offline,
                export_path: Some(path),
                source_run: None,
                cached_output: None,
                audio_export: Some(request),
            },
        )
        .unwrap();
    let (status, _) = failed_prepared_model_task(&mut service, task);
    assert!(status.error.unwrap().contains("Export would clip"));
    assert_eq!(status.phase, ProcessingPhase::Failed);
    for node in &status.graph_nodes {
        // Model inference finishes before streaming. A rejected first block
        // of a longer stream must not complete the remaining native work.
        if fully_rendered || node.node == 3 {
            assert_eq!(node.phase, ProcessingPhase::Complete);
        } else if node.node == target.node {
            assert_eq!(node.phase, ProcessingPhase::Failed);
        }
        assert!(
            node.error.is_none(),
            "encoding error attributed to {node:?}"
        );
    }
    assert_eq!(service.memory_bytes(), 0);
    assert_eq!(fs::read_dir(output_dir).unwrap().count(), 0);
}

#[test]
fn graph_model_failure_keeps_each_nodes_provenance_and_original_operation() {
    let (directory, mut service, track, _) = fixture();
    write_float_wav(
        &track.path,
        2,
        8_000,
        PCM.into_iter().flat_map(|sample| [sample; 2]),
    )
    .unwrap();
    let worker = directory.path().join("models.py");
    fs::write(
        &worker,
        r#"import json, signal, struct, sys
signal.alarm(10)
def read():
    kind, size = struct.unpack('<BI', sys.stdin.buffer.read(5))
    return kind, sys.stdin.buffer.read(size)
def send(kind, payload):
    sys.stdout.buffer.write(struct.pack('<BI', kind, len(payload)) + payload)
kind, payload = read()
request = json.loads(payload)
manifest = json.load(open(request['model_manifest']))
model = dict(id=manifest['id'], sha256=manifest['id'] + '-hash')
send(0, json.dumps(dict(version=request['version'], task_id=request['task_id'], type='progress', completed=0,
                        total=request['frames'], model=model)).encode())
while True:
    kind, payload = read()
    if kind == 2:
        break
    assert kind == 1
    if model['id'] == 'first':
        send(17, payload)
if model['id'] == 'second':
    send(0, json.dumps(dict(version=request['version'], task_id=request['task_id'], type='error',
                            message='second model failed')).encode())
    sys.stdout.buffer.flush()
    raise SystemExit(7)
send(0, json.dumps(dict(version=request['version'], task_id=request['task_id'], type='complete',
                        sample_rate=request['sample_rate'], channels=request['channels'],
                        frames=request['frames'], outputs=dict(speech=True), model=model)).encode())
sys.stdout.buffer.flush()
"#,
    )
    .unwrap();
    let model = |id: &str| {
        let manifest = directory.path().join(format!("{id}.json"));
        fs::write(&manifest, serde_json::json!({"id":id}).to_string()).unwrap();
        GraphNodeKind::Speech {
            config: rivu_format::config::NeuralConfig {
                speech_manifest: Some(manifest),
                speech_interpreter: "python3".into(),
                worker: worker.clone(),
                ..Default::default()
            },
        }
    };
    let task = service
        .start_graph(
            track,
            graph_config(vec![model("first"), model("second")]),
            GraphRequest {
                target: GraphPort { node: 2, port: 0 },
                mode: GraphExecutionMode::Offline,
                export_path: None,
                source_run: None,
                cached_output: None,
                audio_export: None,
            },
        )
        .unwrap();
    let mut nodes = BTreeMap::new();
    loop {
        match next_event(&mut service) {
            Event::GraphNode { status, .. } => {
                nodes.insert(status.node, status);
            }
            Event::Finished { id, result } => {
                assert_eq!(id, task);
                assert!(result.is_err());
                service.finish_task(id).unwrap();
                break;
            }
            Event::Progress { .. } | Event::GraphPrepared { .. } => {}
            _ => panic!("unexpected model graph event"),
        }
    }
    assert_eq!(nodes[&3].phase, ProcessingPhase::Complete);
    assert_eq!(nodes[&3].model_id.as_deref(), Some("first"));
    assert_eq!(nodes[&3].model_sha256.as_deref(), Some("first-hash"));
    assert_eq!(nodes[&4].phase, ProcessingPhase::Failed);
    assert_eq!(nodes[&4].model_id.as_deref(), Some("second"));
    assert_eq!(nodes[&4].model_sha256.as_deref(), Some("second-hash"));
    assert_eq!(
        service.status(task).unwrap().operation,
        ProcessingOperation::Graph
    );
    assert!(service.status(task).unwrap().model_id.is_none());
}

fn preparation_fixture() -> (tempfile::TempDir, Track, Arc<Config>) {
    source_fixture(2, 8_000, PCM)
}

fn preparation_only_model(
    directory: &std::path::Path,
    id: &str,
    backend: &str,
) -> rivu_format::config::NeuralConfig {
    let manifest = directory.join(format!("{id}.json"));
    fs::write(
        &manifest,
        serde_json::json!({"id": id, "operation": "speech", "backend": backend}).to_string(),
    )
    .unwrap();
    rivu_format::config::NeuralConfig {
        speech_manifest: Some(manifest.clone()),
        // Preparation only requires a regular worker file. No worker or model
        // runtime is executed: a later preparation/decode failure must win.
        worker: manifest,
        speech_interpreter: directory.join("must-not-run"),
        ..Default::default()
    }
}

fn failed_prepared_model_task(
    service: &mut Service,
    task: u64,
) -> (ProcessingTaskStatus, Vec<GraphNodeStatus>) {
    let mut nodes = Vec::new();
    loop {
        // Consume real worker events through the same Service reducers as Core.
        // Never copy a worker status or synthesize provenance into the record.
        match next_event(service) {
            Event::Model { id, provenance } => {
                assert_eq!(id, task);
                service.model_provenance(id, provenance);
            }
            Event::GraphNode { id, status } => {
                assert_eq!(id, task);
                service.node_status(id, status.clone());
                nodes.push(status);
            }
            Event::GraphPrepared {
                id,
                mode,
                boundary,
                plan,
            } => {
                assert_eq!(id, task);
                service.graph_prepared(id, mode, boundary, plan);
            }
            Event::Progress {
                id,
                completed,
                total,
                phase,
            } => {
                assert_eq!(id, task);
                service.progress(id, completed, total, phase);
            }
            Event::Finished { id, result } => {
                assert_eq!(id, task);
                let error = result
                    .err()
                    .expect("prepared model task unexpectedly succeeded");
                assert_eq!(service.finish_task(id), Some(false));
                service.outcome(id, ProcessingPhase::Failed, Some(format!("{error:#}")));
                service.retain_terminal(id);
                let published = service.current_projection().unwrap();
                assert_eq!(published.id, task);
                assert_eq!(published.phase, ProcessingPhase::Failed);
                assert_eq!(service.history().last(), Some(published.as_ref()));
                return ((*published).clone(), nodes);
            }
            _ => panic!("unexpected prepared model event"),
        }
    }
}

fn failed_prepared_graph(
    service: &mut Service,
    track: Track,
    config: Arc<Config>,
) -> (ProcessingTaskStatus, Vec<GraphNodeStatus>) {
    let task = service
        .start_graph(
            track,
            config,
            GraphRequest {
                target: GraphPort { node: 2, port: 0 },
                mode: GraphExecutionMode::Offline,
                export_path: None,
                source_run: None,
                cached_output: None,
                audio_export: None,
            },
        )
        .unwrap();
    failed_prepared_model_task(service, task)
}

fn failed_prepared_node<'a>(
    status: &'a ProcessingTaskStatus,
    nodes: &[GraphNodeStatus],
    node: u64,
    id: &str,
) -> &'a GraphNodeStatus {
    let prepared = nodes.iter().find(|entry| entry.node == node).unwrap();
    assert_eq!(prepared.phase, ProcessingPhase::Queued);
    assert_eq!(prepared.model_id.as_deref(), Some(id));
    let terminal = status
        .graph_nodes
        .iter()
        .find(|entry| entry.node == node)
        .unwrap();
    assert_eq!(terminal.phase, ProcessingPhase::Failed);
    assert_eq!(terminal.model_id.as_deref(), Some(id));
    // Python preparation knows the declared identity, not a verified hash.
    assert!(terminal.model_sha256.is_none());
    terminal
}

#[test]
fn model_preparation_provenance_survives_later_graph_preparation_failure() {
    let (directory, track, _) = preparation_fixture();
    // Preparation must fail before source allocation, and no interpreter exists.
    let mut service = Service::with_memory_limit(1);
    let config = graph_config(vec![
        GraphNodeKind::Speech {
            config: preparation_only_model(directory.path(), "prepared-first", "python"),
        },
        GraphNodeKind::Speech {
            config: preparation_only_model(directory.path(), "rejected-second", "unsupported"),
        },
    ]);
    let (status, nodes) = failed_prepared_graph(&mut service, track, config);
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("Unsupported model backend")
    );
    for (node, id) in [(3, "prepared-first"), (4, "rejected-second")] {
        failed_prepared_node(&status, &nodes, node, id);
    }
    assert!(
        status
            .graph_nodes
            .iter()
            .find(|node| node.node == 3)
            .unwrap()
            .error
            .is_none()
    );
    assert!(status.model_id.is_none());
    assert_eq!(service.memory_bytes(), 0);
}

#[test]
fn model_preparation_provenance_survives_graph_decode_failure() {
    let (directory, track, _) = preparation_fixture();
    let mut service = Service::with_memory_limit(1);
    let config = graph_config(
        ["prepared-first", "prepared-second"]
            .into_iter()
            .map(|id| GraphNodeKind::Speech {
                config: preparation_only_model(directory.path(), id, "python"),
            })
            .collect(),
    );
    let (status, nodes) = failed_prepared_graph(&mut service, track, config);
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("PCM memory budget exceeded")
    );
    assert_eq!(nodes.len(), 2);
    for (node, id) in [(3, "prepared-first"), (4, "prepared-second")] {
        let terminal = failed_prepared_node(&status, &nodes, node, id);
        assert!(terminal.error.is_none());
    }
    assert!(status.model_id.is_none());
    assert_eq!(service.memory_bytes(), 0);
}

#[test]
fn first_model_prefix_failures_keep_source_and_node_errors_distinct() {
    use rivu_dsp::equalizer::config::{EqBand, EqualizerConfig};

    let eq = |frequency_hz| GraphNodeKind::Equalizer {
        config: EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                frequency_hz,
                gain_db: 1.0,
                ..Default::default()
            }],
            ..Default::default()
        },
        implementation: EqImplementation::Iir,
    };
    for (kind, sample, limit, failed_node, message) in [
        (
            eq(10_000.0),
            0.125,
            MEMORY_LIMIT,
            Some(3),
            "rate-dependent maximum",
        ),
        (
            GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
            f32::MAX,
            MEMORY_LIMIT,
            Some(3),
            "Nonfinite graph output",
        ),
        (eq(1_000.0), 0.125, 1, None, "PCM memory budget exceeded"),
    ] {
        let (directory, track, _) = source_fixture(2, 8_000, [sample; 16]);
        let mut config = graph_config(vec![
            kind,
            GraphNodeKind::Equalizer {
                config: Default::default(),
                implementation: EqImplementation::Iir,
            },
            GraphNodeKind::Speech {
                config: preparation_only_model(directory.path(), "prepared-model", "python"),
            },
        ]);
        let graph = &mut Arc::get_mut(&mut config).unwrap().processing_graph;
        if matches!(graph.node(3).unwrap().kind, GraphNodeKind::Mix { .. }) {
            graph.edges.push(GraphEdge {
                from: GraphPort { node: 1, port: 0 },
                to: GraphPort { node: 3, port: 1 },
            });
        }
        let mut service = Service::with_memory_limit(limit);
        let (status, nodes) = failed_prepared_graph(&mut service, track, config);
        assert!(status.error.as_deref().unwrap().contains(message));
        let model = failed_prepared_node(&status, &nodes, 5, "prepared-model");
        assert!(model.error.is_none());
        let running: Vec<_> = nodes
            .iter()
            .filter(|node| node.phase == ProcessingPhase::Processing)
            .map(|node| node.node)
            .collect();
        assert_eq!(running.as_slice(), failed_node.as_slice());
        let failures: Vec<_> = nodes
            .iter()
            .filter(|node| node.phase == ProcessingPhase::Failed)
            .collect();
        assert_eq!(failures.len(), failed_node.as_slice().len());
        if let Some(node) = failed_node {
            assert_eq!(failures[0].node, node);
            assert_eq!(failures[0].error, status.error);
            assert_eq!(
                status.graph_nodes.iter().find(|entry| entry.node == node),
                Some(failures[0])
            );
        }
        assert_eq!(service.memory_bytes(), 0);
    }
}

#[test]
fn model_preparation_provenance_survives_standalone_decode_failure() {
    let (directory, track, mut config) = preparation_fixture();
    let mut service = Service::with_memory_limit(1);
    Arc::make_mut(&mut config).neural =
        preparation_only_model(directory.path(), "prepared-standalone", "python");
    let task = service
        .start(track, config, ProcessingOperation::Speech, None)
        .unwrap();
    let (status, nodes) = failed_prepared_model_task(&mut service, task);
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("PCM memory budget exceeded")
    );
    assert_eq!(status.operation, ProcessingOperation::Speech);
    assert_eq!(status.model_id.as_deref(), Some("prepared-standalone"));
    assert!(status.model_sha256.is_none());
    assert!(nodes.is_empty());
    assert_eq!(service.memory_bytes(), 0);
}

fn assert_graph_failure(
    service: &mut Service,
    track: &Track,
    config: Arc<Config>,
    failed_node: u64,
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
                audio_export: None,
            },
        )
        .unwrap();
    let mut processing_seen = false;
    let mut terminal_nodes = Vec::new();
    let task_error = loop {
        match next_event(service) {
            Event::Progress { id, .. } | Event::GraphPrepared { id, .. } => assert_eq!(id, task),
            Event::GraphNode { id, status } => {
                assert_eq!(id, task);
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
                assert_eq!(id, task);
                service.finish_task(service.active_id().unwrap()).unwrap();
                break format!(
                    "{:#}",
                    result.err().expect("graph task unexpectedly succeeded")
                );
            }
            _ => panic!("unexpected graph failure event"),
        }
    };
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
        write_float_wav(&track.path, 1, 8_000, input).unwrap();
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
fn offline_out_of_source_selection_reports_its_denoise_node_even_when_bypassed() {
    use rivu_dsp::denoise::{DenoiseConfig, DenoiseMode};

    for denoise in [
        DenoiseConfig::default(),
        DenoiseConfig {
            mode: DenoiseMode::Profile,
            wet: 0.0,
            fft_size: 256,
            ..Default::default()
        },
    ] {
        let (_directory, mut service, track, _) = fixture();
        let config = graph_config(vec![GraphNodeKind::Denoise {
            config: denoise,
            profile: Some(NoiseSelection {
                start_seconds: 0.0,
                end_seconds: 1.0,
            }),
        }]);
        assert_graph_failure(&mut service, &track, config, 3);
    }
}

#[test]
fn native_graph_decode_budget_failure_reports_requested_output_once() {
    let (_directory, track, _) = source_fixture(1, 8_000, PCM);
    let mut service = Service::with_memory_limit(1);
    assert_graph_failure(&mut service, &track, graph_config(Vec::new()), 2);
    assert_eq!(service.memory_bytes(), 0);
}

#[test]
fn retained_graph_failures_report_the_responsible_node_once() {
    use rivu_dsp::equalizer::config::{EqBand, EqualizerConfig};

    let eq = |frequency_hz| GraphNodeKind::Equalizer {
        config: EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                frequency_hz,
                gain_db: 1.0,
                ..Default::default()
            }],
            ..Default::default()
        },
        implementation: EqImplementation::Iir,
    };
    for (kind, limit, failed_node) in [
        (eq(10_000.0), MEMORY_LIMIT, 3),
        (eq(1_000.0), 1, 2),
        (
            GraphNodeKind::Denoise {
                config: Default::default(),
                profile: Some(NoiseSelection {
                    start_seconds: 0.0,
                    end_seconds: 1.0,
                }),
            },
            MEMORY_LIMIT,
            3,
        ),
    ] {
        let (_directory, service, _track, _) = fixture();
        let retained = &service.results[0];
        let input = retained.outputs.vocals.as_ref().unwrap();
        let mut status = service.status(retained.id).unwrap().clone();
        let target = GraphPort { node: 2, port: 0 };
        let config = graph_config(vec![kind]);
        let graph = &config.processing_graph;
        status.operation = ProcessingOperation::Graph;
        status.settings.graph = Some(graph.clone());
        status.settings.graph_target = Some(target);
        status.settings.graph_requested_mode = Some(GraphExecutionMode::Offline);
        status.settings.graph_mode = Some(GraphExecutionMode::Offline);
        status.graph_nodes = graph
            .ancestors(target)
            .unwrap()
            .into_iter()
            .map(|node| GraphNodeStatus {
                node,
                phase: ProcessingPhase::Queued,
                error: None,
                model_id: None,
                model_sha256: None,
            })
            .collect();
        let budget = audio::PcmBudget::new(limit);
        let (tx, rx) = async_channel::unbounded();
        let mut progress = ProgressReporter::new(&tx, status.id);
        // Exercise the retained-PCM executor directly, without model weights or Python.
        let error = graph::execute(
            &graph::ExecutionGraph::new(&graph.validated().unwrap(), target).unwrap(),
            input.clone(),
            Arc::clone(&budget),
            &mut status,
            &Cancellation::new().unwrap(),
            &mut progress,
            BTreeMap::new(),
        )
        .err()
        .expect("graph unexpectedly succeeded");
        let error = format!("{error:#}");
        let mut terminal_nodes = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::GraphNode { status, .. } = event
                && status.phase.is_terminal()
            {
                terminal_nodes.push(status);
            }
        }
        assert_eq!(
            terminal_nodes.len(),
            1,
            "missing or duplicate terminal nodes"
        );
        let failed = &terminal_nodes[0];
        assert_eq!(failed.node, failed_node);
        assert_eq!(failed.phase, ProcessingPhase::Failed);
        assert_eq!(failed.error.as_deref(), Some(error.as_str()));
        assert_eq!(
            status
                .graph_nodes
                .iter()
                .find(|node| node.node == failed_node),
            Some(failed)
        );
        assert_eq!(budget.used_bytes(), 0);
    }
}

#[test]
fn model_capability_rejections_precede_source_pcm_allocation() {
    for graph in [false, true] {
        for (metadata, rate, post_filter, device, expected) in [
            (
                serde_json::json!({"backend":"onnx"}),
                8000,
                false,
                rivu_format::config::NeuralDevice::Cuda,
                "CPU only",
            ),
            (
                serde_json::json!({"backend":"onnx"}),
                8000,
                true,
                rivu_format::config::NeuralDevice::Cpu,
                "post_filter",
            ),
            (
                serde_json::json!({
                    "version": 1,
                    "operation": "speech",
                    "backend": "onnx",
                    "id": "unsupported",
                    "model": "model.onnx",
                    "license_evidence": "weight-license.txt",
                    "files": {
                        "model.onnx": "0".repeat(64),
                        "weight-license.txt": "0".repeat(64),
                    },
                }),
                8000,
                false,
                rivu_format::config::NeuralDevice::Cpu,
                if cfg!(feature = "onnx") {
                    "Unsupported native speech model"
                } else {
                    "onnx feature"
                },
            ),
            (
                serde_json::json!({"backend":"unsupported"}),
                8000,
                false,
                rivu_format::config::NeuralDevice::Cpu,
                "Unsupported model backend",
            ),
            (
                serde_json::json!({"operation":"separate"}),
                8000,
                false,
                rivu_format::config::NeuralDevice::Cpu,
                "manifest operation",
            ),
            (
                serde_json::json!({"id":"MossFormer2_SE_48K"}),
                8000,
                true,
                rivu_format::config::NeuralDevice::Cpu,
                "post_filter",
            ),
            (
                serde_json::json!({"id":"DeepFilterNet3"}),
                384000,
                false,
                rivu_format::config::NeuralDevice::Cpu,
                "8000..192000",
            ),
        ] {
            let (directory, track, mut config) = source_fixture(2, rate, PCM);
            let manifest = directory.path().join("manifest.json");
            fs::write(&manifest, metadata.to_string()).unwrap();
            let neural = rivu_format::config::NeuralConfig {
                speech_manifest: Some(manifest.clone()),
                worker: manifest,
                device,
                speech_post_filter: post_filter,
                ..Default::default()
            };
            // A decoded source cannot fit: the capability error proves rejection
            // happened before full-source allocation, not merely before inference.
            let mut service = Service::with_memory_limit(1);
            let error = if graph {
                completed_graph(
                    &mut service,
                    &track,
                    graph_config(vec![GraphNodeKind::Speech { config: neural }]),
                )
                .err()
                .unwrap()
            } else {
                Arc::make_mut(&mut config).neural = neural;
                service
                    .start(track, config, ProcessingOperation::Speech, None)
                    .unwrap();
                loop {
                    if let Event::Finished { id, result } = next_event(&mut service) {
                        service.finish_task(id).unwrap();
                        break result.err().unwrap();
                    }
                }
            };
            assert!(
                format!("{error:#}").contains(expected),
                "graph={graph}: {error:#}"
            );
            assert_eq!(service.memory_bytes(), 0);
        }
    }
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
    write_float_wav(
        &track.path,
        3,
        8_000,
        (0..16).flat_map(|_| [0.1_f32, 0.2, 0.3]),
    )
    .unwrap();
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
fn cached_export_cancelled_before_encoding_has_no_executed_nodes() {
    bounded(|| {
        let (directory, mut service, track, config) = fixture();
        let result = &service.results[0];
        let cached = CachedGraphOutput {
            pcm: result.outputs.vocals.as_ref().unwrap().clone(),
            stamp: result.stamp,
        };
        let destination = directory.path().join("cancelled.wav");
        let target = GraphPort { node: 2, port: 0 };
        let release = block_reclamation(&mut service);
        let id = service
            .start_graph(
                track,
                config,
                GraphRequest {
                    target,
                    mode: GraphExecutionMode::Offline,
                    export_path: Some(destination.clone()),
                    source_run: Some(1),
                    cached_output: Some(cached),
                    audio_export: None,
                },
            )
            .unwrap();
        service.cancel(id).unwrap();
        unblock_reclamation(&mut service, release);

        let Event::Finished {
            id: finished,
            result: Err(error),
        } = next_event(&mut service)
        else {
            panic!("expected cancellation before encoding");
        };
        assert_eq!(finished, id);
        assert_eq!(service.finish_task(id), Some(true));
        service.outcome(id, ProcessingPhase::Cancelled, Some(error.to_string()));
        let status = service.status(id).unwrap();
        assert_eq!(status.settings.graph_target, Some(target));
        assert_eq!(status.graph_run_id, Some(1));
        assert!(status.graph_nodes.is_empty());
        assert!(!destination.exists());
    });
}

fn exhaust_failed_spawns<T: std::fmt::Debug>(
    service: &mut Service,
    mut start: impl FnMut(&mut Service) -> Result<T>,
) -> std::sync::mpsc::Sender<()> {
    let release = block_reclamation(service);
    service.reclaimer.as_mut().unwrap().fail_spawns = true;
    for _ in 0..64 {
        let error = start(service).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected processing worker spawn failure")
        );
        assert!(service.active_id().is_none());
    }
    assert_eq!(service.sender.len(), service.sender.capacity().unwrap());
    service.reclaimer.as_mut().unwrap().fail_spawns = false;
    release
}

#[test]
fn failed_processing_spawns_do_not_consume_retirement_capacity() {
    bounded(|| {
        let (_directory, mut service, track, config) = fixture();
        let budget = Arc::clone(&service.budget);
        let release = exhaust_failed_spawns(&mut service, |service| {
            service.start(
                track.clone(),
                config.clone(),
                ProcessingOperation::Denoise,
                None,
            )
        });
        let status = service
            .start(track, config, ProcessingOperation::Denoise, None)
            .unwrap();
        unblock_reclamation(&mut service, release);
        loop {
            match next_event(&mut service) {
                Event::Progress { .. } => {}
                Event::Finished { id, result } => {
                    assert_eq!(id, status);
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
fn stem_mix_aligns_sliced_stems_across_storage_blocks() {
    let (_directory, mut service, _track, _config) = fixture();
    let layout: Arc<[audio::Channel]> =
        Arc::from([audio::Channel::FrontLeft, audio::Channel::FrontRight]);
    let frames = 9_001;
    let stem = |start, sign: f32| {
        let mut builder =
            audio::PcmBuilder::new(48_000, Arc::clone(&layout), Arc::clone(&service.budget))
                .unwrap();
        let data: Vec<_> = (0..frames + start)
            .flat_map(|frame| {
                let sample = (frame % 32) as f32 / 32.0;
                [sample * sign, sample * sign * 0.5]
            })
            .collect();
        builder.push(&data).unwrap();
        builder
            .finish()
            .unwrap()
            .slice_frames(start, frames)
            .unwrap()
    };
    let vocals = stem(7, 1.0);
    let accompaniment = stem(13, -1.0);
    let outputs = &mut service.results.front_mut().unwrap().outputs;
    outputs.vocals = Some(vocals);
    outputs.accompaniment = Some(accompaniment);
    service.start_mix(1, 7, 0.0, 0.0).unwrap();
    let pcm = match next_event(&mut service) {
        Event::MixFinished { id, result, .. } => {
            service.finish_task(id).unwrap();
            result.unwrap()
        }
        _ => panic!("unexpected stem mix event"),
    };
    assert_eq!(pcm.frames(), frames);
    assert_eq!(pcm.sample_rate(), 48_000);
    assert_eq!(pcm.layout(), &layout);
    let expected: Vec<_> = (0..frames)
        .flat_map(|frame| {
            let sample = (((frame + 7) % 32) as f32 - ((frame + 13) % 32) as f32) / 32.0;
            [sample, sample * 0.5]
        })
        .collect();
    assert_eq!(samples(&pcm), expected);
}

#[test]
fn failed_mix_spawns_do_not_consume_retirement_capacity() {
    bounded(|| {
        let (_directory, mut service, _track, _config) = fixture();
        let budget = Arc::clone(&service.budget);
        let release =
            exhaust_failed_spawns(&mut service, |service| service.start_mix(1, 7, 0.0, 0.0));
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
        service
            .start(track, config, ProcessingOperation::Denoise, None)
            .unwrap_err();
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
        assert!(service.cancel(status + 1).is_err());
        service.cancel(status).unwrap();
        unblock_reclamation(&mut service, release);
        loop {
            match next_event(&mut service) {
                Event::Progress { .. } => {}
                Event::Finished { id, result } => {
                    assert_eq!(id, status);
                    assert!(result.is_err());
                    service.finish_task(service.active_id().unwrap()).unwrap();
                    break;
                }
                _ => panic!("unexpected event from cancelled processing"),
            }
        }
        assert_eq!(service.memory_bytes(), before);
        let retry = completed(&mut service, &track, &config);
        assert!(retry.status.id > status);
        assert_eq!(samples(retry.outputs.denoised.as_ref().unwrap()), PCM);
    });
}

#[test]
fn pcm_budget_rejects_inflight_allocation_without_creating_processing_directory() {
    let (directory, track, config) = source_fixture(1, 8_000, PCM);
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
            assert!(result.is_err());
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
    prepare_export(pcm, &cancelled, &cancel, |done, _| {
        if done > 0 {
            wrote_frames = true;
            cancel.store(true, Ordering::Release);
        }
    })
    .err()
    .unwrap();
    assert!(wrote_frames);
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
fn retained_wav_export_respects_pcm_slice_across_blocks() {
    use audio::Channel::{FrontLeft, FrontRight};

    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("slice.wav");
    let cancel = AtomicBool::new(false);
    let input: Vec<_> = (0..20_000).map(|sample| sample as f32 / 32768.0).collect();
    let mut builder = audio::PcmBuilder::new(
        48_000,
        Arc::from([FrontLeft, FrontRight]),
        audio::PcmBudget::new(128 * 1024),
    )
    .unwrap();
    builder.push(&input).unwrap();
    let pcm = builder.finish().unwrap();
    for (start, frames) in [(4079, 4200), (10_000, 0)] {
        let view = pcm.slice_frames(start, frames).unwrap();
        let mut progress = 0;
        let (temporary, _) = prepare_export(&view, &destination, &cancel, |done, total| {
            assert_eq!(total, frames);
            assert!((progress..=total).contains(&done));
            progress = done;
        })
        .unwrap();
        assert_eq!(progress, frames);
        let decoded = hound::WavReader::open(temporary.path())
            .unwrap()
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            decoded,
            input[(start * 2) as usize..((start + frames) * 2) as usize]
        );
    }
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
    service.accept_result(Box::new(result));
    service.release_unpinned_audio();
    assert!(service.results()[0].outputs.vocals.is_some());
    assert!(service.results()[1].outputs.denoised.is_none());
    assert!(
        service
            .status(service.results()[1].id)
            .unwrap()
            .outputs
            .is_none()
    );
    assert_eq!(service.results()[1].track.path, track.path);
    assert_eq!(
        service
            .status(service.results()[1].id)
            .unwrap()
            .settings
            .denoise,
        config.denoise
    );
    let result = completed_graph(&mut service, &track, config.clone()).unwrap();
    let graph_id = result.status.id;
    let frozen = result.status.settings.graph.clone();
    assert!(!result.graph_outputs.is_empty());
    service.accept_result(Box::new(result));
    service.release_unpinned_audio();
    let status = service.status(graph_id).unwrap();
    assert_eq!(status.settings.graph, frozen);
    assert!(status.graph_actions_available);
    assert!(status.graph_outputs.is_empty());
    assert!(
        service
            .results()
            .iter()
            .find(|result| result.id == graph_id)
            .unwrap()
            .graph_outputs
            .is_empty()
    );
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
fn resource_directory_projects_eviction_without_overwriting_outcome_or_pinned_audio() {
    let (_directory, mut service, track, config) = fixture();
    service.retain_terminal(1);
    for _ in 0..4 {
        let result = completed(&mut service, &track, &config);
        let id = result.status.id;
        service.accept_result(Box::new(result));
        service.retain_terminal(id);
    }
    assert_eq!(service.results().len(), 4);
    assert!(service.results().iter().any(|result| result.id == 1));
    assert!(!service.results().iter().any(|result| result.id == 2));
    assert!(service.status(1).unwrap().outputs.as_ref().unwrap().vocals);
    let evicted = service.status(2).unwrap();
    assert_eq!(evicted.phase, ProcessingPhase::Complete);
    assert!(evicted.error.is_none());
    assert!(evicted.outputs.is_none());
    let first = service.current_projection().unwrap();
    let second = service.current_projection().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(first.outputs.as_ref().unwrap().denoised);
    assert_eq!(service.audition_id(), Some(1));
}

#[test]
fn expired_pinned_live_run_stops_without_replacing_newer_current() {
    let (_directory, mut service, track, config) = fixture();
    let target = GraphPort { node: 2, port: 0 };
    let live = service
        .start_graph(
            track.clone(),
            config.clone(),
            GraphRequest {
                target,
                mode: GraphExecutionMode::Realtime,
                export_path: None,
                source_run: None,
                cached_output: None,
                audio_export: None,
            },
        )
        .unwrap();
    let result = loop {
        if let Event::Finished { id, result } = next_event(&mut service) {
            assert_eq!(id, live);
            service.finish_task(id).unwrap();
            break result.unwrap();
        }
    };
    let snapshot = result.runner.as_ref().unwrap().snapshot();
    let frozen = result.status.settings.graph.clone();
    service.accept_result(result);
    service.select_graph_audition(
        live,
        1,
        None,
        snapshot,
        target,
        GraphExecutionMode::Realtime,
    );
    service.retain_terminal(live);
    for _ in 0..33 {
        let result = completed(&mut service, &track, &config);
        let id = result.status.id;
        service.accept_result(Box::new(result));
        service.retain_terminal(id);
    }
    assert!(!service.history().any(|status| status.id == live));
    assert_eq!(service.audition_id(), Some(live));
    assert_eq!(service.status(live).unwrap().settings.graph, frozen);
    assert!(service.status(live).unwrap().graph_actions_available);
    let newer = service.current_projection().unwrap();
    service.stop_live_audition(live);
    let stopped = service.status(live).unwrap();
    assert_eq!(stopped.phase, ProcessingPhase::Cancelled);
    assert_eq!(stopped.error.as_deref(), Some("Live audition stopped"));
    assert!(
        stopped
            .graph_nodes
            .iter()
            .all(|node| node.phase != ProcessingPhase::Processing)
    );
    assert!(Arc::ptr_eq(&newer, &service.current_projection().unwrap()));
    assert_eq!(service.history().next_back().unwrap().id, live);
    assert_eq!(service.history().len(), 32);
    let position = service.history().position(|status| status.id == newer.id);
    service.retain_terminal(newer.id);
    assert_eq!(
        service.history().position(|status| status.id == newer.id),
        position
    );
}

#[test]
fn published_processing_arc_survives_record_and_availability_changes() {
    let (_directory, mut service, _track, _config) = fixture();
    service.retain_terminal(1);
    let published = service.current_projection().unwrap();
    let expected = published.as_ref().clone();
    service.start_mix(1, 7, 0.0, 0.0).unwrap();
    assert_eq!(service.current_projection().unwrap().id, 1);
    assert_eq!(
        service.status(1).unwrap().phase,
        ProcessingPhase::Processing
    );
    let processing = service.current_projection().unwrap();
    service.progress(1, 2, Some(4), ProcessingPhase::Processing);
    assert_eq!(processing.completed, expected.completed);
    assert_eq!(service.status(1).unwrap().completed, 2);
    let pcm = match next_event(&mut service) {
        Event::MixFinished { id, result, .. } => {
            service.finish_task(id).unwrap();
            result.unwrap()
        }
        _ => panic!("unexpected stem mix event"),
    };
    service.retire_pcm(pcm);
    service.outcome(1, ProcessingPhase::Failed, Some("audition rejected".into()));
    service.retain_terminal(1);
    assert_eq!(service.history().len(), 1);
    assert_eq!(
        service.history().next().unwrap(),
        service.status(1).unwrap()
    );
    service.clear_audition();
    service.release_unpinned_audio();
    assert!(service.status(1).unwrap().outputs.is_none());
    assert_eq!(published.as_ref(), &expected);
    let released = service.current_projection().unwrap();
    service.release_unpinned_audio();
    assert!(Arc::ptr_eq(
        &released,
        &service.current_projection().unwrap()
    ));
}

#[test]
fn processing_status_expires_after_last_membership_despite_published_arc() {
    let (_directory, mut service, track, config) = fixture();
    service.retain_terminal(1);
    let published = service.current_projection().unwrap();
    let expected = published.as_ref().clone();
    service.clear_audition();
    for _ in 0..31 {
        let result = completed(&mut service, &track, &config);
        let id = result.status.id;
        service.accept_result(Box::new(result));
        service.retain_terminal(id);
    }
    assert!(!service.results().iter().any(|result| result.id == 1));
    assert_ne!(service.current_projection().unwrap().id, 1);
    assert_eq!(service.history().next().unwrap().id, 1);
    assert!(service.status(1).is_some());
    let result = completed(&mut service, &track, &config);
    let id = result.status.id;
    service.accept_result(Box::new(result));
    assert!(service.status(1).is_some());
    service.retain_terminal(id);
    assert!(!service.history().any(|status| status.id == 1));
    assert!(service.status(1).is_none());
    assert_eq!(published.as_ref(), &expected);
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
                audio_export: None,
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
    service.clear_audition();
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
    assert!(
        service
            .status(service.results()[0].id)
            .unwrap()
            .outputs
            .is_none()
    );
    assert_eq!(
        service.memory_bytes(),
        result.outputs.denoised.as_ref().unwrap().retained_bytes()
    );
}

#[test]
fn native_decoder_streams_fit_one_output_budget_including_profile_passes() {
    let (directory, track, _) = source_fixture(
        1,
        8_000,
        (0..20000).map(|frame| (frame as f32 * 0.07).sin() * 0.2),
    );
    let data: Vec<f32> = (0..20000)
        .map(|frame| (frame as f32 * 0.07).sin() * 0.2)
        .collect();
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
    write_float_wav(&track.path, 1, 48_000, data.iter().copied()).unwrap();
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
                &graph::ExecutionGraph::new(
                    &result
                        .status
                        .settings
                        .graph
                        .as_ref()
                        .unwrap()
                        .validated()
                        .unwrap(),
                    GraphPort { node: 2, port: 0 },
                )
                .unwrap(),
                input,
                Arc::clone(&service.budget),
                &mut result.status.clone(),
                &Cancellation::new().unwrap(),
                &mut progress,
                BTreeMap::new(),
            )
            .unwrap();
            let retained = samples(&retained.outputs[&GraphPort { node: 2, port: 0 }].pcm);
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
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn native_file_exports_match_retained_timeline_without_pcm_budget() {
    use rivu_dsp::{
        denoise::{DenoiseConfig, DenoiseMode},
        equalizer::config::{EqBand, EqualizerConfig, FilterKind},
    };
    for frames in [1, 63, 257, 4093] {
        let (_directory, track, _) = source_fixture(
            1,
            8_000,
            (0..frames).map(|index| (index as f32 * 0.17).cos() * 0.3),
        );
        let eq = EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                kind: FilterKind::LowPass,
                frequency_hz: 1500.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fir = GraphNodeKind::Equalizer {
            config: eq.clone(),
            implementation: EqImplementation::LinearPhase { taps: 257 },
        };
        let mut cases = vec![
            vec![],
            vec![GraphNodeKind::Equalizer {
                config: eq,
                implementation: EqImplementation::Iir,
            }],
            vec![fir.clone()],
            vec![fir.clone(), fir.clone()],
            vec![GraphNodeKind::Denoise {
                config: DenoiseConfig {
                    mode: DenoiseMode::Stationary,
                    fft_size: 256,
                    ..Default::default()
                },
                profile: None,
            }],
        ];
        if frames == 4093 {
            let profile = GraphNodeKind::Denoise {
                config: DenoiseConfig {
                    mode: DenoiseMode::Profile,
                    fft_size: 256,
                    ..Default::default()
                },
                profile: Some(NoiseSelection {
                    start_seconds: 0.01,
                    end_seconds: 0.04,
                }),
            };
            cases.push(vec![fir.clone(), profile.clone()]);
            cases.push(vec![fir, profile.clone(), profile]);
        }
        for (case, kinds) in cases.into_iter().enumerate() {
            let config = graph_config(kinds);
            let mut reference = Service::new();
            let result = completed_graph(&mut reference, &track, Arc::clone(&config)).unwrap();
            let expected = samples(&result.graph_outputs[&GraphPort { node: 2, port: 0 }].pcm);
            let destination = track.path.with_file_name(format!("export-{case}.wav"));
            let mut service = Service::with_memory_limit(0);
            service
                .start_graph(
                    track.clone(),
                    config,
                    GraphRequest {
                        target: GraphPort { node: 2, port: 0 },
                        mode: GraphExecutionMode::Offline,
                        export_path: Some(destination.clone()),
                        source_run: None,
                        cached_output: None,
                        audio_export: None,
                    },
                )
                .unwrap();
            loop {
                if let Event::Finished { result, .. } = next_event(&mut service) {
                    service.finish_task(service.active_id().unwrap()).unwrap();
                    let result = result.unwrap();
                    assert!(result.graph_outputs.is_empty());
                    assert_eq!(service.memory_bytes(), 0);
                    assert_eq!(result.status.frames, Some(frames));
                    assert!(!destination.exists(), "worker must not publish");
                    let actual =
                        hound::WavReader::open(result.pending_export.as_ref().unwrap().0.path())
                            .unwrap()
                            .samples::<f32>()
                            .collect::<std::result::Result<Vec<_>, _>>()
                            .unwrap();
                    assert_eq!(actual, expected, "frames={frames}, case={case}");
                    break;
                }
            }
        }
    }
}
