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
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(10), service.receiver.recv())
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

fn block_reclamation(service: &mut Service, track: &Track, config: &Arc<Config>) {
    let result = completed(service, track, config);
    for _ in 0..service.sender.max_capacity() {
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
    for _ in 0..service.sender.max_capacity() {
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
        assert_eq!(service.sender.capacity(), 0);
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
        assert_eq!(service.sender.capacity(), 0);
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
