use super::*;

pub(super) fn source(directory: &Path) -> Vec<f32> {
    let samples = (0..4096)
        .flat_map(|i| {
            let v = (i as f32 * 0.07).sin() * 1.2;
            [v, -v * 0.5]
        })
        .collect::<Vec<_>>();
    let mut writer = hound::WavWriter::create(
        directory.join("1.wav"),
        hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for &sample in &samples {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    samples
}
fn next_processing_event(core: &mut Core, timeout: Duration) -> Result<crate::processing::Event> {
    futures::executor::block_on(async {
        rivu_protocol::transport::timeout(timeout, core.processing.receiver.recv())
            .await
            .context("Timed out waiting for processing event")?
            .context("Processing event channel disconnected")
    })
}

fn completion(core: &mut Core) -> crate::processing::Event {
    loop {
        let event = next_processing_event(core, Duration::from_secs(10)).unwrap();
        if matches!(event, crate::processing::Event::Finished { .. }) {
            return event;
        }
    }
}
fn complete(core: &mut Core) {
    loop {
        let event = next_processing_event(core, Duration::from_secs(10)).unwrap();
        core.processing_event(event);
        if core.processing.active.is_none() {
            break;
        }
    }
}

fn terminal_wait(core: &mut Core, id: u64) -> ProcessingTaskStatus {
    let (handle, requests) = super::task_wait::wait_handle(core);
    let mut wait = Box::pin(handle.wait_task(TaskTarget::Processing { id }));
    assert!(wait.as_mut().now_or_never().is_none());
    core.business_event(dispatch::BusinessEvent::Request(
        requests.try_recv().unwrap(),
    ));
    let TaskOutcome::Processing(status) = wait.as_mut().now_or_never().unwrap().unwrap() else {
        panic!("expected processing outcome");
    };
    *status
}

fn prepare_mix(core: &mut Core) {
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    let mut event = completion(core);
    let crate::processing::Event::Finished {
        result: Ok(result), ..
    } = &mut event
    else {
        panic!("native processing did not produce PCM");
    };
    // Use real native PCM for both stems without requiring a prepared neural model.
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
    core.processing_event(event);
}

fn begin_mix(core: &mut Core, id: u64) {
    let mut status = core.processing_status(id).unwrap();
    crate::processing::mix(&mut core.processing, id, core.generation, 0.0, 0.0).unwrap();
    status.phase = ProcessingPhase::Processing;
    status.error = None;
    core.state.system.processing = Some(status);
}

fn mix_completion(core: &mut Core) -> crate::processing::Event {
    let event = next_processing_event(core, Duration::from_secs(10)).unwrap();
    assert!(matches!(
        event,
        crate::processing::Event::MixFinished { .. }
    ));
    event
}

#[test]
fn mix_terminal_replaces_history_and_survives_current_and_history_eviction() {
    let (directory, mut core) = fixture();
    source(directory.path());
    prepare_mix(&mut core);
    let frozen = core.processing_status(1).unwrap();
    begin_mix(&mut core, 1);
    let event = mix_completion(&mut core);
    core.processing.cancel(1).unwrap();
    core.processing_event(event);
    let cancelled = core.processing_status(1).unwrap();
    assert_eq!(cancelled.phase, ProcessingPhase::Cancelled);
    assert_eq!(terminal_wait(&mut core, 1), cancelled);
    assert_eq!(cancelled.outputs, frozen.outputs);
    assert_eq!(cancelled.settings, frozen.settings);
    assert_eq!(core.processing_results.len(), 1);

    // A later successful Mix replaces the same receipt, rather than leaving the
    // old cancellation in history or creating another receipt for the same ID.
    begin_mix(&mut core, 1);
    complete(&mut core);
    let complete_status = core.processing_status(1).unwrap();
    assert_eq!(complete_status.phase, ProcessingPhase::Complete);
    assert!(complete_status.error.is_none());
    assert_eq!(core.processing_results.len(), 1);
    assert_eq!(core.processing_results[0], complete_status);

    // Cancel once more so falling back to the original Complete is observable.
    begin_mix(&mut core, 1);
    let event = mix_completion(&mut core);
    core.processing.cancel(1).unwrap();
    core.processing_event(event);
    let latest = core.processing_status(1).unwrap();
    core.start_processing(1, ProcessingOperation::Speech, None)
        .unwrap();
    assert_eq!(core.processing_status(1).unwrap(), latest);
    assert_eq!(terminal_wait(&mut core, 1), latest);
    complete(&mut core);
    // Failed native jobs age the receipt without evicting retained audio.
    for _ in 1..MAX_RETAINED_SCAN_RESULTS {
        core.start_processing(1, ProcessingOperation::Speech, None)
            .unwrap();
        complete(&mut core);
    }
    assert!(!core.processing_results.iter().any(|status| status.id == 1));
    assert_eq!(core.processing.results.len(), 1);
    assert_eq!(core.processing_status(1).unwrap(), latest);
    assert_eq!(terminal_wait(&mut core, 1), latest);
}

#[test]
fn native_and_mix_worker_panics_commit_failure_finish_waits_and_release_admission() {
    for mixing in [false, true] {
        let (directory, mut core) = fixture();
        source(directory.path());
        if mixing {
            prepare_mix(&mut core);
            core.processing.panic_after_mix = true;
            begin_mix(&mut core, 1);
        } else {
            core.processing.panic_after_processing = true;
            core.start_processing(1, ProcessingOperation::Denoise, None)
                .unwrap();
        }
        let event = if mixing {
            mix_completion(&mut core)
        } else {
            completion(&mut core)
        };
        let (handle, requests) = super::task_wait::wait_handle(&core);
        let mut wait = Box::pin(handle.wait_task(TaskTarget::Processing { id: 1 }));
        assert!(wait.as_mut().now_or_never().is_none());
        core.business_event(dispatch::BusinessEvent::Request(
            requests.try_recv().unwrap(),
        ));
        // The real worker has unwound and sent its error, but the owner still
        // holds admission and must commit before any waiter can finish.
        assert!(wait.as_mut().now_or_never().is_none());
        core.processing_event(event);
        let TaskOutcome::Processing(status) = wait.as_mut().now_or_never().unwrap().unwrap() else {
            panic!("expected failed processing outcome");
        };
        assert_eq!(status.phase, ProcessingPhase::Failed);
        assert!(status.error.as_deref().unwrap().contains("worker panicked"));
        assert!(core.processing.active.is_none());
        assert_eq!(terminal_wait(&mut core, 1), *status);
        if mixing {
            let result = &core.processing.results[0];
            assert!(result.mix.is_none());
        } else {
            assert!(core.processing.results.is_empty());
        }
        core.start_processing(1, ProcessingOperation::Denoise, None)
            .unwrap();
        assert_eq!(core.processing_status(1).unwrap(), *status);
        complete(&mut core);
        assert_eq!(
            core.processing_status(2).unwrap().phase,
            ProcessingPhase::Complete
        );
        assert_eq!(terminal_wait(&mut core, 1), *status);
    }
}

#[test]
fn shutdown_mix_retires_queued_output_and_keeps_latest_retained_outcome() {
    let (directory, mut core) = fixture();
    source(directory.path());
    prepare_mix(&mut core);
    let frozen = core.processing_status(1).unwrap();
    begin_mix(&mut core, 1);
    let event = mix_completion(&mut core);
    let crate::processing::Event::MixFinished {
        result: Ok(path), ..
    } = &event
    else {
        panic!("mix did not produce output");
    };
    let path = path.clone();
    assert!(core.processing.sender.try_send(event).is_ok());
    core.shutdown_processing();
    assert!(!path.exists());
    let cancelled = core.processing_status(1).unwrap();
    assert_eq!(cancelled.phase, ProcessingPhase::Cancelled);
    assert_eq!(cancelled.outputs, frozen.outputs);
    assert_eq!(cancelled.settings, frozen.settings);
    assert_eq!(core.processing_results.len(), 1);
    core.state.system.processing = None;
    assert_eq!(terminal_wait(&mut core, 1), cancelled);
    core.processing_results.clear();
    assert_eq!(terminal_wait(&mut core, 1), cancelled);
}

fn recording_final_snapshot(
    core: &mut Core,
    snapshot: audio::AudioSnapshot,
) -> Receiver<AudioCommand> {
    let (commands, receive) = async_channel::bounded(64);
    let previous = core.engine.replace_commands(commands);
    let (record, recorded) = crossbeam_channel::unbounded();
    thread::spawn(move || {
        let _previous = previous;
        while let Ok(command) = receive.recv_blocking() {
            match command {
                AudioCommand::StopAndSnapshot(reply) => {
                    let (recorded_reply, _) = oneshot::channel();
                    let _ = record.send(AudioCommand::StopAndSnapshot(recorded_reply));
                    let _ = reply.send(snapshot);
                }
                command => {
                    let _ = record.send(command);
                }
            }
        }
    });
    recorded
}

#[test]
fn offline_job_preserves_native_float_frames_and_does_not_add_library_entries() {
    let (directory, mut core) = fixture();
    let expected = source(directory.path());
    let before = core.store.track_count().unwrap();
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    assert!(
        core.start_processing(1, ProcessingOperation::Denoise, None)
            .is_err()
    );
    complete(&mut core);
    let status = core.processing_status(1).unwrap();
    assert_eq!(status.phase, ProcessingPhase::Complete);
    assert_eq!(status.frames, Some(4096));
    let output = status.outputs.unwrap().denoised.unwrap();
    let actual = hound::WavReader::open(output)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(core.store.track_count().unwrap(), before);
    assert_eq!(
        core.processing.results[0].status.settings.denoise,
        core.state.system.config.denoise
    );
}
#[test]
fn scratch_preparation_failure_is_terminal_and_can_be_retried() {
    let (directory, mut core) = fixture();
    source(directory.path());
    let scratch = directory.path().join("processing-data");
    std::fs::write(&scratch, b"not a directory").unwrap();
    core.processing = crate::processing::Service::new(scratch.clone());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Queued
    );
    complete(&mut core);
    let failed = core.processing_status(1).unwrap();
    assert_eq!(failed.phase, ProcessingPhase::Failed);
    assert!(failed.outputs.is_none());
    assert!(failed.graph_outputs.is_empty());
    std::fs::remove_file(&scratch).unwrap();
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    assert_eq!(
        core.processing_status(2).unwrap().phase,
        ProcessingPhase::Complete
    );
}
#[test]
fn cancellation_after_physical_completion_still_prevents_publication() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    let event = completion(&mut core);
    core.processing.cancel(1).unwrap();
    core.processing_event(event);
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Cancelled
    );
    assert!(core.processing.results.is_empty());
}

#[test]
fn uncertain_database_rejects_held_processing_completion_before_source_read_or_export() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    let mut event = completion(&mut core);
    let destination = directory.path().join("must-not-publish.wav");
    if let crate::processing::Event::Finished {
        result: Ok(result), ..
    } = &mut event
    {
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        std::fs::write(temporary.path(), b"staged export").unwrap();
        result.pending_export = Some((temporary, destination.clone()));
    } else {
        panic!("expected successful physical processing completion");
    }
    core.database_outcome_uncertain();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch("ALTER TABLE tracks RENAME TO quarantined_tracks")
        .unwrap();
    core.processing_event(event);
    let status = core.processing_status(1).unwrap();
    assert_eq!(status.phase, ProcessingPhase::Failed);
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("Database outcome is uncertain")
    );
    assert!(core.processing.active.is_none());
    assert!(core.processing.results.is_empty());
    assert!(!destination.exists());
}

#[test]
fn changed_source_between_completion_and_commit_is_rejected() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    let event = completion(&mut core);
    std::fs::write(directory.path().join("1.wav"), b"replacement").unwrap();
    core.processing_event(event);
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Failed
    );
    assert!(core.processing.results.is_empty());
}
#[test]
fn retained_output_directories_and_statuses_are_bounded() {
    let (directory, mut core) = fixture();
    source(directory.path());
    for _ in 0..34 {
        core.start_processing(1, ProcessingOperation::Denoise, None)
            .unwrap();
        complete(&mut core);
    }
    assert_eq!(core.processing.results.len(), 4);
    assert_eq!(core.processing_results.len(), 32);
    assert!(core.processing_status(1).is_none());
    assert!(core.processing_status(3).unwrap().outputs.is_none());
    assert!(
        core.audition_processing(1, AuditionMode::Denoised, 0.0, 0.0)
            .is_err()
    );
}
#[test]
fn equalizer_preset_import_bounds_bytes_and_rejects_non_regular_inputs() {
    let (directory, mut core) = fixture();
    let path = directory.path().join("preset.json");
    let preset = rivu_dsp::equalizer::config::EqualizerConfig {
        preamp_db: -3.0,
        ..Default::default()
    };
    let mut bytes = serde_json::to_vec(&preset).unwrap();
    bytes.resize(65536, b' ');
    std::fs::write(&path, &bytes).unwrap();
    core.equalizer_edit(EqualizerEdit::ImportPreset {
        path: path.clone(),
        name: "bounded".into(),
    })
    .unwrap();
    let imported = core
        .state
        .system
        .config
        .equalizer_presets
        .iter()
        .find(|preset| preset.name == "bounded")
        .unwrap();
    assert_eq!(imported.config, preset);
    let accepted = core.state.system.config.clone();
    bytes.push(b' ');
    std::fs::write(&path, bytes).unwrap();
    let error = core
        .equalizer_edit(EqualizerEdit::ImportPreset {
            path: path.clone(),
            name: "oversized".into(),
        })
        .unwrap_err();
    assert!(error.to_string().contains("Preset exceeds 64 KiB"));
    assert_eq!(core.state.system.config, accepted);

    std::fs::remove_file(&path).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (completed, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = core.equalizer_edit(EqualizerEdit::ImportPreset {
            path,
            name: "pipe".into(),
        });
        completed
            .send((core.state.system.config.clone(), result))
            .unwrap();
    });
    // Do not join on timeout: a regression must fail rather than hang the suite.
    let (config, result) = receive.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert!(format!("{:#}", result.unwrap_err()).contains("not an ordinary file"));
    assert_eq!(config, accepted);
}

#[test]
fn equalizer_and_denoise_edits_merge_on_the_core_owner() {
    let (_directory, mut core) = fixture();
    let _commands = recording_audio(&mut core);
    core.equalizer_edit(EqualizerEdit::AddBand {
        band: rivu_dsp::equalizer::config::EqBand::default(),
    })
    .unwrap();
    core.equalizer_edit(EqualizerEdit::BandField {
        index: 0,
        field: EqBandEdit::GainDb(3.0),
    })
    .unwrap();
    core.equalizer_edit(EqualizerEdit::BandField {
        index: 0,
        field: EqBandEdit::FrequencyHz(2500.0),
    })
    .unwrap();
    assert_eq!(core.state.system.config.equalizer.bands[0].gain_db, 3.0);
    core.denoise_edit(DenoisePatch {
        strength: Some(2.0),
        ..Default::default()
    })
    .unwrap();
    core.denoise_edit(DenoisePatch {
        wet: Some(0.25),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(core.state.system.config.denoise.strength, 2.0);
    assert_eq!(core.state.system.config.denoise.wet, 0.25);
}
#[test]
fn empty_equalizer_batches_use_log_uniform_neutral_cell_centers() {
    use rivu_dsp::equalizer::config::{EqBand, MAX_BANDS};
    for count in [1, 6, MAX_BANDS] {
        let (_directory, mut core) = fixture();
        let (commands, receive) = async_channel::bounded(64);
        let _idle_commands = core.engine.replace_commands(commands);
        assert!(core.state.current_track.is_none());
        core.equalizer_edit(EqualizerEdit::AddBands { count })
            .unwrap();
        let equalizer = &core.state.system.config.equalizer;
        assert!(!equalizer.enabled);
        assert_eq!(equalizer.preamp_db, 0.0);
        assert_eq!(equalizer.output_gain_db, 0.0);
        assert_eq!(equalizer.bands.len(), count);
        for (index, band) in equalizer.bands.iter().enumerate() {
            let expected_hz = 20.0 * 1000.0_f64.powf((index as f64 + 0.5) / count as f64);
            assert!((band.frequency_hz - expected_hz).abs() < 1e-8);
            assert_eq!(
                band,
                &EqBand {
                    frequency_hz: band.frequency_hz,
                    ..Default::default()
                }
            );
        }
        match receive.try_recv().unwrap() {
            AudioCommand::Equalizer(delivered) => assert_eq!(delivered.as_ref(), equalizer),
            _ => panic!("Batch did not deliver an equalizer update"),
        }
        assert!(receive.try_recv().is_err());
        assert_eq!(
            Config::load(&core.state.system.config_path).unwrap(),
            *core.state.system.config
        );
    }
}

#[test]
fn equalizer_batches_fill_largest_log_gaps_without_rewriting_edited_bands() {
    use rivu_dsp::equalizer::config::{ChannelTarget, EqBand, EqualizerConfig, FilterKind};
    let (_directory, mut core) = fixture();
    let _commands = recording_audio(&mut core);
    let original = EqualizerConfig {
        enabled: false,
        preamp_db: -4.0,
        output_gain_db: 2.0,
        bands: vec![
            EqBand {
                kind: FilterKind::LowShelf,
                frequency_hz: 2000.0,
                gain_db: 4.0,
                q: 1.3,
                shelf_slope: 0.5,
                solo: true,
                channels: ChannelTarget::Selected(vec![0]),
                ..Default::default()
            },
            EqBand {
                enabled: false,
                kind: FilterKind::Notch,
                frequency_hz: 100.0,
                gain_db: -5.0,
                q: 2.0,
                stages: 2,
                ..Default::default()
            },
        ],
    };
    core.equalizer_edit(EqualizerEdit::Replace {
        config: original.clone(),
    })
    .unwrap();
    core.equalizer_edit(EqualizerEdit::AddBands { count: 2 })
        .unwrap();
    let equalizer = &core.state.system.config.equalizer;
    assert_eq!(&equalizer.bands[..2], original.bands.as_slice());
    assert_eq!(equalizer.enabled, original.enabled);
    assert_eq!(equalizer.preamp_db, original.preamp_db);
    assert_eq!(equalizer.output_gain_db, original.output_gain_db);
    assert_eq!(equalizer.bands.len(), 4);
    for (band, expected_hz) in equalizer.bands[2..]
        .iter()
        .zip([200_000.0_f64.sqrt(), 40_000_000.0_f64.sqrt()])
    {
        assert!((band.frequency_hz - expected_hz).abs() < 1e-8);
        assert_eq!(
            band,
            &EqBand {
                frequency_hz: band.frequency_hz,
                ..Default::default()
            }
        );
    }
}

#[test]
fn resizing_equalizer_preserves_prefix_and_rejects_invalid_counts_atomically() {
    use rivu_dsp::equalizer::config::{EqBand, MAX_BANDS};
    let (_directory, mut core) = fixture();
    let (commands, receive) = async_channel::bounded(64);
    let _idle_commands = core.engine.replace_commands(commands);
    let first = EqBand {
        frequency_hz: 731.25,
        gain_db: -3.5,
        ..Default::default()
    };
    core.equalizer_edit(EqualizerEdit::AddBand {
        band: first.clone(),
    })
    .unwrap();
    receive.try_recv().unwrap();
    for count in [3, 1, MAX_BANDS, 0] {
        core.equalizer_edit(EqualizerEdit::ResizeBands { count })
            .unwrap();
        let eq = &core.state.system.config.equalizer;
        assert_eq!(eq.bands.len(), count);
        if count > 0 {
            assert_eq!(eq.bands[0], first);
        }
        assert!(matches!(
            receive.try_recv().unwrap(),
            AudioCommand::Equalizer(_)
        ));
        assert!(receive.try_recv().is_err());
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(&core.state.system.config_path).unwrap())
                .unwrap();
        assert_eq!(saved["equalizer"]["bands"].as_array().unwrap().len(), count);
    }
    let baseline = core.state.system.config.clone();
    let saved = std::fs::read(&core.state.system.config_path).unwrap();
    for count in [MAX_BANDS + 1, usize::MAX] {
        assert!(
            core.equalizer_edit(EqualizerEdit::ResizeBands { count })
                .is_err()
        );
        assert_eq!(core.state.system.config, baseline);
        assert_eq!(
            std::fs::read(&core.state.system.config_path).unwrap(),
            saved
        );
        assert!(receive.try_recv().is_err());
    }
}

#[test]
fn invalid_equalizer_batch_counts_preserve_memory_disk_and_audio_atomically() {
    use rivu_dsp::equalizer::config::MAX_BANDS;
    let (_directory, mut core) = fixture();
    let (commands, receive) = async_channel::bounded(64);
    let _idle_commands = core.engine.replace_commands(commands);
    core.equalizer_edit(EqualizerEdit::AddBand {
        band: rivu_dsp::equalizer::config::EqBand::default(),
    })
    .unwrap();
    assert!(matches!(
        receive.try_recv().unwrap(),
        AudioCommand::Equalizer(_)
    ));
    let baseline = core.state.system.config.clone();
    let saved = std::fs::read(&core.state.system.config_path).unwrap();
    for count in [0, MAX_BANDS, MAX_BANDS + 1, usize::MAX] {
        assert!(
            core.equalizer_edit(EqualizerEdit::AddBands { count })
                .is_err()
        );
        assert_eq!(core.state.system.config, baseline);
        assert_eq!(
            std::fs::read(&core.state.system.config_path).unwrap(),
            saved
        );
        assert!(receive.try_recv().is_err());
    }
    core.equalizer_edit(EqualizerEdit::AddBands {
        count: MAX_BANDS - 1,
    })
    .unwrap();
    assert_eq!(core.state.system.config.equalizer.bands.len(), MAX_BANDS);
    assert!(matches!(
        receive.try_recv().unwrap(),
        AudioCommand::Equalizer(_)
    ));
    let baseline = core.state.system.config.clone();
    let saved = std::fs::read(&core.state.system.config_path).unwrap();
    assert!(
        core.equalizer_edit(EqualizerEdit::AddBands { count: 1 })
            .is_err()
    );
    assert_eq!(core.state.system.config, baseline);
    assert_eq!(
        std::fs::read(&core.state.system.config_path).unwrap(),
        saved
    );
    assert!(receive.try_recv().is_err());
}

#[test]
fn equalizer_batch_generation_uses_current_track_rate_or_unknown_rate_fallback() {
    use rivu_dsp::equalizer::config::{MAX_BANDS, MAX_FREQUENCY_RATE_RATIO};
    for sample_rate in [0, 8000, 22050, 48000, 192000] {
        let (_directory, mut core) = fixture();
        let _commands = recording_audio(&mut core);
        let mut track = core.track(1).unwrap();
        track.sample_rate = sample_rate;
        core.state.current_track = Some(Arc::new(track));
        let maximum_hz = if sample_rate == 0 {
            20_000.0
        } else {
            20_000.0_f64.min(f64::from(sample_rate) * MAX_FREQUENCY_RATE_RATIO)
        };
        core.equalizer_edit(EqualizerEdit::AddBands { count: MAX_BANDS })
            .unwrap();
        for (index, band) in core.state.system.config.equalizer.bands.iter().enumerate() {
            let expected_hz =
                20.0 * (maximum_hz / 20.0).powf((index as f64 + 0.5) / MAX_BANDS as f64);
            assert!((band.frequency_hz - expected_hz).abs() < 1e-8);
            assert!((20.0..=maximum_hz).contains(&band.frequency_hz));
        }
    }
}

#[test]
fn rate_bounded_gap_filling_preserves_existing_out_of_range_bands() {
    use rivu_dsp::equalizer::config::{EqBand, MAX_FREQUENCY_RATE_RATIO};
    let (_directory, mut core) = fixture();
    let _commands = recording_audio(&mut core);
    let mut track = core.track(1).unwrap();
    track.sample_rate = 8000;
    core.state.current_track = Some(Arc::new(track));
    let original = EqBand {
        frequency_hz: 18_000.0,
        gain_db: 5.0,
        ..Default::default()
    };
    core.equalizer_edit(EqualizerEdit::AddBand {
        band: original.clone(),
    })
    .unwrap();
    core.equalizer_edit(EqualizerEdit::AddBands { count: 3 })
        .unwrap();
    let bands = &core.state.system.config.equalizer.bands;
    assert_eq!(bands[0], original);
    let maximum_hz = 8000.0 * MAX_FREQUENCY_RATE_RATIO;
    assert!((bands[1].frequency_hz - (20.0 * maximum_hz).sqrt()).abs() < 1e-8);
    assert!(
        bands[1..].iter().all(|band| {
            (20.0..=maximum_hz).contains(&band.frequency_hz) && band.gain_db == 0.0
        })
    );
}

#[test]
fn unusable_equalizer_batch_frequency_range_is_rejected_atomically() {
    let (_directory, mut core) = fixture();
    let (commands, receive) = async_channel::bounded(64);
    let _idle_commands = core.engine.replace_commands(commands);
    let mut track = core.track(1).unwrap();
    track.sample_rate = 40;
    core.state.current_track = Some(Arc::new(track));
    let baseline = core.state.system.config.clone();
    assert!(
        core.equalizer_edit(EqualizerEdit::AddBands { count: 3 })
            .unwrap_err()
            .to_string()
            .contains("no usable equalizer frequency range")
    );
    assert_eq!(core.state.system.config, baseline);
    assert!(!core.state.system.config_path.exists());
    assert!(receive.try_recv().is_err());
}

#[test]
fn audition_preserves_queue_source_identity_and_cumulative_heard_stats() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let commands = recording_audio(&mut core);
    let queue_id = core.queue_ids[0];
    core.start(queue_id, false).unwrap();
    loop {
        if matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Load { .. }
        ) {
            break;
        }
    }
    core.playback.as_mut().unwrap().started = true;
    core.playback.as_mut().unwrap().heard = 2.5;
    core.playback.as_mut().unwrap().counted = true;
    core.state.playback.position = 0.04;
    let generation = core.generation;
    let commands = recording_final_snapshot(
        &mut core,
        audio::AudioSnapshot {
            generation,
            position_seconds: 0.065,
            listened_seconds: 2.52,
        },
    );
    let source = core.state.current_track.clone();
    core.audition_processing(1, AuditionMode::Denoised, 0.0, 0.0)
        .unwrap();
    let command = loop {
        let command = commands.recv_timeout(Duration::from_secs(2)).unwrap();
        if matches!(command, AudioCommand::Audition { .. }) {
            break command;
        }
    };
    assert!(matches!(
        command,
        AudioCommand::Audition {
            range: None,
            start_seconds: 0.065,
            listened_seconds: 2.52,
            source_info: Some(_),
            ..
        }
    ));
    assert_eq!(core.state.playback.position, 0.065);
    assert_eq!(core.state.current_track, source);
    assert_eq!(core.state.queue.current_id, Some(queue_id));
    assert!(core.playback.as_ref().unwrap().counted);
}
#[test]
fn manifest_patch_distinguishes_json_clear_and_preserves_binary_absence() {
    let patch: NeuralPatch = serde_json::from_str(r#"{"speech_manifest":null}"#).unwrap();
    assert_eq!(patch.speech_manifest, Some(None));
    assert_eq!(patch.separation_manifest, None);
    let bytes = postcard::to_allocvec(&NeuralPatch::default()).unwrap();
    let patch: NeuralPatch = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(patch.speech_manifest, None);
}

#[test]
fn audition_starts_original_queue_identity_without_creating_processed_tracks() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let before = core.store.track_count().unwrap();
    let commands = recording_audio(&mut core);
    core.audition_processing(1, AuditionMode::Denoised, 0.0, 0.0)
        .unwrap();
    assert_eq!(core.state.current_track.as_ref().unwrap().id, 1);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.store.track_count().unwrap(), before);
    let command = loop {
        let command = commands.recv_timeout(Duration::from_secs(2)).unwrap();
        if matches!(command, AudioCommand::Audition { .. }) {
            break command;
        }
    };
    assert!(matches!(
        command,
        AudioCommand::Audition {
            start_seconds: 0.0,
            listened_seconds: 0.0,
            ..
        }
    ));
}

#[test]
fn decoder_hint_change_rejects_retained_auditions_before_starting_playback() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.edit_track(1, None, None, None, Some(DecoderHint::Auto))
        .unwrap();
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    core.command(Command::EditTrack {
        track_id: 1,
        title: None,
        artist: None,
        album: None,
        decoder_hint: Some(DecoderHint::Native),
    })
    .unwrap();
    let generation = core.generation;
    let current = core.state.queue.current_id;
    for mode in [
        AuditionMode::Original,
        AuditionMode::Denoised,
        AuditionMode::Mix,
    ] {
        let error = core.audition_processing(1, mode, 0.0, 0.0).unwrap_err();
        assert!(error.to_string().contains("Processing result is stale"));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.queue.current_id, current);
        assert_eq!(core.generation, generation);
        assert!(core.processing.active.is_none());
    }
}

#[test]
fn decoder_hint_change_before_mix_handoff_discards_the_rendered_mix() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.edit_track(1, None, None, None, Some(DecoderHint::Auto))
        .unwrap();
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let result = &mut core.processing.results[0];
    let stem = result
        .status
        .outputs
        .as_ref()
        .unwrap()
        .denoised
        .clone()
        .unwrap();
    result.status.outputs.as_mut().unwrap().vocals = Some(stem.clone());
    result.status.outputs.as_mut().unwrap().accompaniment = Some(stem);
    let _commands = recording_audio(&mut core);
    core.audition_processing(1, AuditionMode::Mix, 0.0, 0.0)
        .unwrap();
    let event = next_processing_event(&mut core, Duration::from_secs(10)).unwrap();
    let crate::processing::Event::MixFinished {
        result: Ok(path), ..
    } = &event
    else {
        panic!("expected a completed mix");
    };
    let path = path.clone();
    core.command(Command::EditTrack {
        track_id: 1,
        title: None,
        artist: None,
        album: None,
        decoder_hint: Some(DecoderHint::Native),
    })
    .unwrap();
    let generation = core.generation;
    core.processing_event(event);
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Failed
    );
    assert!(core.processing.results[0].mix.is_none());
    assert!(core.processing.audition.is_none());
    assert_eq!(core.generation, generation);
    assert!(!path.exists());
}

#[test]
fn background_mix_applies_actual_gains_without_clipping_or_new_track_identity() {
    let (directory, mut core) = fixture();
    let original = source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let result = &mut core.processing.results[0];
    let stem = result
        .status
        .outputs
        .as_ref()
        .unwrap()
        .denoised
        .clone()
        .unwrap();
    result.status.outputs.as_mut().unwrap().vocals = Some(stem.clone());
    result.status.outputs.as_mut().unwrap().accompaniment = Some(stem);
    let _commands = recording_audio(&mut core);
    core.audition_processing(1, AuditionMode::Mix, -6.0, 0.0)
        .unwrap();
    let event = next_processing_event(&mut core, Duration::from_secs(10)).unwrap();
    assert!(matches!(
        event,
        crate::processing::Event::MixFinished { .. }
    ));
    core.processing_event(event);
    let mixed = core.processing.results[0].mix.as_ref().unwrap();
    let actual = hound::WavReader::open(mixed)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual.len(), original.len());
    let gain = 1.0 + 10.0_f32.powf(-6.0 / 20.0);
    for (actual, source) in actual.iter().zip(&original) {
        assert!((actual - source * gain).abs() < 2e-6);
    }
    assert!(actual.iter().any(|sample| sample.abs() > 1.0));
    assert_eq!(core.state.current_track.as_ref().unwrap().id, 1);
}

#[test]
fn profile_mode_requires_an_explicit_noise_selection_and_preserves_frame_count() {
    let (directory, mut core) = fixture();
    source(directory.path());
    Arc::make_mut(&mut core.state.system.config).denoise.mode =
        rivu_dsp::denoise::DenoiseMode::Profile;
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Failed
    );
    core.start_processing(
        1,
        ProcessingOperation::Denoise,
        Some(NoiseSelection {
            start_seconds: 0.0,
            end_seconds: 0.06,
        }),
    )
    .unwrap();
    complete(&mut core);
    let status = core.processing_status(2).unwrap();
    assert_eq!(status.phase, ProcessingPhase::Complete);
    assert_eq!(status.frames, Some(4096));
}

#[test]
fn rendered_descriptor_rejects_symlink_target_round_trip() {
    use std::{os::unix::fs::symlink, sync::atomic::AtomicBool};
    let directory = tempfile::tempdir().unwrap();
    source(directory.path());
    let alternate = directory.path().join("alternate.wav");
    let mut writer = hound::WavWriter::create(
        &alternate,
        hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for _ in 0..128 {
        writer.write_sample(0.125_f32).unwrap();
    }
    writer.finalize().unwrap();
    let alias = directory.path().join("source.wav");
    symlink(directory.path().join("1.wav"), &alias).unwrap();
    let admitted = crate::processing::SourceStamp::read(&alias).unwrap();
    std::fs::remove_file(&alias).unwrap();
    symlink(&alternate, &alias).unwrap();
    let rendered = audio::SourceRenderer::open(
        &alias,
        None,
        DecoderHint::Native,
        1,
        false,
        &AtomicBool::new(false),
    )
    .unwrap()
    .render(
        &directory.path().join("render.wav"),
        u64::MAX,
        &AtomicBool::new(false),
    )
    .unwrap();
    std::fs::remove_file(&alias).unwrap();
    symlink(directory.path().join("1.wav"), &alias).unwrap();
    assert_eq!(
        crate::processing::SourceStamp::read(&alias).unwrap(),
        admitted
    );
    assert!(
        admitted
            .validate_rendered(rendered.source_identity)
            .is_err()
    );
}

#[test]
fn retained_audition_remains_queryable_after_terminal_history_eviction() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let _commands = recording_audio(&mut core);
    core.audition_processing(1, AuditionMode::Denoised, 0.0, 0.0)
        .unwrap();
    let path = core
        .processing_status(1)
        .unwrap()
        .outputs
        .unwrap()
        .denoised
        .unwrap();
    for _ in 0..33 {
        core.start_processing(1, ProcessingOperation::Denoise, None)
            .unwrap();
        complete(&mut core);
    }
    assert!(!core.processing_results.iter().any(|status| status.id == 1));
    let ViewResponse::ProcessingTask(Some(task)) =
        core.view(&Query::ProcessingTask { id: 1 }).unwrap()
    else {
        panic!("Retained audition disappeared from its query");
    };
    assert_eq!(task.phase, ProcessingPhase::Complete);
    assert_eq!(task.outputs.unwrap().denoised, Some(path.clone()));
    assert!(path.is_file());
    core.audition_processing(1, AuditionMode::Original, 0.0, 0.0)
        .unwrap();
}

#[test]
fn neural_cancellation_reaps_interpreter_that_closed_stdout_without_exiting() {
    let (directory, mut core) = fixture();
    source(directory.path());
    let worker = directory.path().join("close_stdout.sh");
    std::fs::write(
        &worker,
        "exec 1>&-\nprintf '%s' \"$$\" > \"${0%/*}/worker.ready\"\nwhile :; do :; done\n",
    )
    .unwrap();
    let manifest = directory.path().join("manifest.json");
    std::fs::write(&manifest, "{}").unwrap();
    let config = Arc::make_mut(&mut core.state.system.config);
    config.neural.worker = worker;
    config.neural.speech_interpreter = "/bin/sh".into();
    config.neural.speech_manifest = Some(manifest);
    core.command(Command::StartProcessing {
        track_id: 1,
        operation: ProcessingOperation::Speech,
        profile: None,
    })
    .unwrap();
    let ready = directory.path().join("worker.ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&ready)
            && pid.parse::<u32>().is_ok_and(|pid| pid > 0)
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "Interpreter did not reach the closed-stdout fixture"
        );
        thread::yield_now();
    };
    core.command(Command::CancelProcessing { id: 1 }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let event = loop {
        match core
            .processing
            .receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(event @ crate::processing::Event::Finished { .. }) => break Some(event),
            Ok(event) => core.processing_event(event),
            Err(_) => break None,
        }
    };
    if event.is_none() {
        // Ensure a regression reports failure rather than leaving its real child alive.
        let _ = std::process::Command::new("/bin/kill")
            .arg("-KILL")
            .arg(&pid)
            .status();
    }
    assert!(
        event.is_some(),
        "Cancellation stalled after interpreter stdout EOF"
    );
    core.processing_event(event.unwrap());
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Cancelled
    );
    assert!(
        !Path::new("/proc").join(pid).exists(),
        "Interpreter was not reaped"
    );
}

#[test]
fn neural_unwind_reaps_live_interpreter_before_failure_event_and_owner_retirement() {
    let (directory, mut core) = fixture();
    source(directory.path());
    let worker = directory.path().join("panic_progress.sh");
    std::fs::write(
        &worker,
        "printf '%s' \"$$\" > \"${0%/*}/worker.ready\"\nprintf '%s\\n' '{\"version\":2,\"task_id\":\"1\",\"type\":\"progress\",\"completed\":0}'\nwhile :; do :; done\n",
    )
    .unwrap();
    let manifest = directory.path().join("manifest.json");
    std::fs::write(&manifest, "{}").unwrap();
    let config = Arc::make_mut(&mut core.state.system.config);
    config.neural.worker = worker;
    config.neural.speech_interpreter = "/bin/sh".into();
    config.neural.speech_manifest = Some(manifest);
    core.processing.panic_on_neural_progress = true;
    core.command(Command::StartProcessing {
        track_id: 1,
        operation: ProcessingOperation::Speech,
        profile: None,
    })
    .unwrap();
    let (handle, requests) = super::task_wait::wait_handle(&core);
    let mut wait = Box::pin(handle.wait_task(TaskTarget::Processing { id: 1 }));
    assert!(wait.as_mut().now_or_never().is_none());
    core.business_event(dispatch::BusinessEvent::Request(
        requests.try_recv().unwrap(),
    ));
    let event = completion(&mut core);
    // The shell cannot exit by itself: its real progress event triggers unwind
    // while it is alive. Reaping must precede sending this failure event, not
    // merely precede the next owner admission or happen during final shutdown.
    let pid = std::fs::read_to_string(directory.path().join("worker.ready"))
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let mut exit = 0;
    // SAFETY: pid identifies this task's child and exit is writable; WNOHANG
    // never blocks if a regression leaves the interpreter running.
    let observed = unsafe { libc::waitpid(pid, &mut exit, libc::WNOHANG) };
    let error = std::io::Error::last_os_error();
    if observed == 0 {
        // Do not leave a live child behind when the regression fails.
        // SAFETY: the unreaped child is still owned by this test process.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, &mut exit, 0);
        }
    }
    assert_eq!(
        observed, -1,
        "interpreter remained live or unreaped at failure delivery"
    );
    assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    assert!(core.processing.active.is_some());
    assert!(wait.as_mut().now_or_never().is_none());
    core.processing_event(event);
    let TaskOutcome::Processing(status) = wait.as_mut().now_or_never().unwrap().unwrap() else {
        panic!("expected processing failure");
    };
    assert_eq!(status.phase, ProcessingPhase::Failed);
    assert!(status.error.as_deref().unwrap().contains("worker panicked"));
    assert!(core.processing.active.is_none());
    assert!(core.processing.results.is_empty());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    assert_eq!(
        core.processing_status(2).unwrap().phase,
        ProcessingPhase::Complete
    );
}

#[test]
fn audition_rejects_stop_snapshot_from_another_generation() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let commands = recording_audio(&mut core);
    let queue_id = core.queue_ids[0];
    core.start(queue_id, false).unwrap();
    loop {
        if matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Load { .. }
        ) {
            break;
        }
    }
    let generation = core.generation + 1;
    let _commands = recording_final_snapshot(
        &mut core,
        audio::AudioSnapshot {
            generation,
            position_seconds: 0.065,
            listened_seconds: 2.52,
        },
    );
    let error = core
        .audition_processing(1, AuditionMode::Denoised, 0.0, 0.0)
        .unwrap_err();
    assert!(error.to_string().contains("different playback generation"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
}

#[test]
fn relative_band_edits_merge_repeated_steps_without_restoring_the_other_axis() {
    let (_directory, mut core) = fixture();
    let _commands = recording_audio(&mut core);
    core.equalizer_edit(EqualizerEdit::AddBand {
        band: rivu_dsp::equalizer::config::EqBand::default(),
    })
    .unwrap();
    for _ in 0..2 {
        core.equalizer_edit(EqualizerEdit::BandField {
            index: 0,
            field: EqBandEdit::GainSteps(1),
        })
        .unwrap();
        core.equalizer_edit(EqualizerEdit::BandField {
            index: 0,
            field: EqBandEdit::FrequencySteps {
                steps: 1,
                maximum_hz: 20000.0,
            },
        })
        .unwrap();
    }
    let band = &core.state.system.config.equalizer.bands[0];
    assert_eq!(band.gain_db, 1.0);
    assert!((band.frequency_hz - 1000.0 * 2.0_f64.powf(2.0 / 12.0)).abs() < 1e-9);
    core.equalizer_edit(EqualizerEdit::BandField {
        index: 0,
        field: EqBandEdit::FrequencySteps {
            steps: 100,
            maximum_hz: 2000.0,
        },
    })
    .unwrap();
    core.equalizer_edit(EqualizerEdit::BandField {
        index: 0,
        field: EqBandEdit::GainSteps(100),
    })
    .unwrap();
    let band = &core.state.system.config.equalizer.bands[0];
    assert_eq!(band.frequency_hz, 2000.0);
    assert_eq!(band.gain_db, 24.0);
}

fn graph_port(node: u64, port: u16) -> GraphPort {
    GraphPort { node, port }
}

fn finished_graph_pcm(
    mut runner: audio::GraphRunner,
    mut pcm: Vec<f32>,
    frames: usize,
) -> Vec<f32> {
    let channels = runner.channels();
    let skip = runner.latency_frames() * channels;
    for block in pcm.chunks_mut(256 * channels) {
        runner.process_interleaved(block).unwrap();
    }
    let mut tail = vec![0.0; 256 * channels];
    for _ in 0..runner.tail_frames().div_ceil(256) {
        let count = runner.finish_interleaved(&mut tail).unwrap();
        pcm.extend_from_slice(&tail[..count * channels]);
    }
    assert_eq!(runner.finish_interleaved(&mut tail).unwrap(), 0);
    pcm[skip..skip + frames * channels].to_vec()
}

#[test]
fn graph_cascaded_nonflat_fir_preserves_source_edge_context() {
    use rivu_dsp::equalizer::config::{EqBand, EqualizerConfig, FilterKind};
    use rivu_dsp::equalizer::fir::FirEqualizer;
    for intermediate_iir in [false, true] {
        let (directory, mut core) = fixture();
        let mut pcm = vec![0.0_f32; 513 * 2];
        pcm[0] = 0.75;
        pcm[1] = -0.25;
        pcm[1024] = -0.5;
        pcm[1025] = 0.625;
        let mut writer = hound::WavWriter::create(
            directory.path().join("1.wav"),
            hound::WavSpec {
                channels: 2,
                sample_rate: 48000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for &sample in &pcm {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        let config = EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                kind: FilterKind::LowPass,
                frequency_hz: 5000.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut graph = ProcessingGraph {
            next_id: 6,
            ..Default::default()
        };
        for id in [3, 4] {
            graph.nodes.push(GraphNode {
                id,
                position: [0.0; 2],
                kind: GraphNodeKind::Equalizer {
                    config: config.clone(),
                    implementation: EqImplementation::LinearPhase { taps: 65 },
                },
            });
        }
        let edges = if intermediate_iir {
            graph.nodes.push(GraphNode {
                id: 5,
                position: [0.0; 2],
                kind: GraphNodeKind::Equalizer {
                    config: EqualizerConfig {
                        enabled: true,
                        preamp_db: -3.0,
                        ..Default::default()
                    },
                    implementation: EqImplementation::Iir,
                },
            });
            vec![(1, 3), (3, 5), (5, 4), (4, 2)]
        } else {
            vec![(1, 3), (3, 4), (4, 2)]
        };
        graph.edges = edges
            .into_iter()
            .map(|(from, to)| GraphEdge {
                from: graph_port(from, 0),
                to: graph_port(to, 0),
            })
            .collect();
        // Reference: retain the complete causal intermediate, compensate once.
        let mut first = FirEqualizer::new(&config, 48000, 2, 65).unwrap();
        let mut second = FirEqualizer::new(&config, 48000, 2, 65).unwrap();
        let latency = first.latency_frames() + second.latency_frames();
        let mut expected = pcm.clone();
        expected.resize(pcm.len() + latency * 2, 0.0);
        first.process_interleaved(&mut expected).unwrap();
        if intermediate_iir {
            for sample in &mut expected {
                *sample *= 10.0_f32.powf(-3.0 / 20.0);
            }
        }
        second.process_interleaved(&mut expected).unwrap();
        let expected = &expected[latency * 2..latency * 2 + pcm.len()];
        let live = finished_graph_pcm(
            audio::GraphRunner::compile(&graph, graph_port(2, 0), 48000, 2).unwrap(),
            pcm.clone(),
            513,
        );
        for (&actual, &expected) in live.iter().zip(expected) {
            assert!((actual - expected).abs() < 2e-6);
        }
        Arc::make_mut(&mut core.state.system.config).processing_graph = graph;
        core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
            .unwrap();
        complete(&mut core);
        let status = core.processing_status(1).unwrap();
        assert_eq!(
            status.phase,
            ProcessingPhase::Complete,
            "{:?}",
            status.error
        );
        assert_eq!(status.frames, Some(513));
        let output = status
            .graph_outputs
            .iter()
            .find(|o| o.port == graph_port(2, 0))
            .unwrap();
        let actual = hound::WavReader::open(&output.path)
            .unwrap()
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(actual.len(), pcm.len());
        for (index, (&actual, &expected)) in actual.iter().zip(expected).enumerate() {
            assert!(
                (actual - expected).abs() < 2e-6,
                "intermediate_iir={intermediate_iir}, sample={index}: offline={actual}, causal={expected}"
            );
        }
    }
}

#[test]
fn graph_cached_fir_boundary_matches_uncut_causal_graph() {
    let (directory, mut core) = fixture();
    let pcm = source(directory.path());
    let mut graph = ProcessingGraph {
        next_id: 5,
        ..Default::default()
    };
    for id in [3, 4] {
        graph.nodes.push(GraphNode {
            id,
            position: [0.0; 2],
            kind: GraphNodeKind::Equalizer {
                config: rivu_dsp::equalizer::config::EqualizerConfig {
                    enabled: true,
                    bands: vec![rivu_dsp::equalizer::config::EqBand {
                        kind: rivu_dsp::equalizer::config::FilterKind::LowPass,
                        frequency_hz: 5000.0,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                implementation: EqImplementation::LinearPhase { taps: 65 },
            },
        });
    }
    graph.edges = [(1, 3), (3, 4), (4, 2)]
        .into_iter()
        .map(|(from, to)| GraphEdge {
            from: graph_port(from, 0),
            to: graph_port(to, 0),
        })
        .collect();
    let expected = finished_graph_pcm(
        audio::GraphRunner::compile(&graph, graph_port(2, 0), 48000, 2).unwrap(),
        pcm.clone(),
        pcm.len() / 2,
    );
    Arc::make_mut(&mut core.state.system.config).processing_graph = graph.clone();
    core.processing_graph_run(1, graph_port(3, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let status = core.processing_status(1).unwrap();
    assert_eq!(
        status.phase,
        ProcessingPhase::Complete,
        "{:?}",
        status.error
    );
    let boundary = status
        .graph_outputs
        .iter()
        .find(|o| o.port == graph_port(3, 0))
        .unwrap();
    let context = boundary
        .context
        .as_ref()
        .expect("FIR boundary must retain its guards");
    assert!(context.start_frame < 0);
    let cached = hound::WavReader::open(&context.path)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    let mut suffix = audio::GraphRunner::compile_from_boundary(
        &graph,
        graph_port(2, 0),
        graph_port(3, 0),
        48000,
        2,
    )
    .unwrap();
    suffix
        .set_source_context(context.start_frame, (pcm.len() / 2) as u64)
        .unwrap();
    assert_eq!(suffix.published_frames(), Some((pcm.len() / 2) as u64));
    let actual = finished_graph_pcm(suffix, cached, pcm.len() / 2);
    assert_eq!(actual.len(), pcm.len());
    for (index, (&actual, &expected)) in actual.iter().zip(&expected).enumerate() {
        assert!(
            (actual - expected).abs() < 2e-6,
            "sample={index}: cached boundary={actual}, uncut={expected}"
        );
    }
}

#[test]
fn graph_hybrid_profile_boundary_preserves_fir_guards_and_source_identity() {
    let (directory, mut core) = fixture();
    let pcm = source(directory.path());
    let commands = recording_audio(&mut core);
    let mut graph = ProcessingGraph {
        next_id: 6,
        ..Default::default()
    };
    for id in [3, 4] {
        graph.nodes.push(GraphNode {
            id,
            position: [0.0; 2],
            kind: GraphNodeKind::Equalizer {
                config: rivu_dsp::equalizer::config::EqualizerConfig {
                    enabled: true,
                    bands: vec![rivu_dsp::equalizer::config::EqBand {
                        kind: rivu_dsp::equalizer::config::FilterKind::LowPass,
                        frequency_hz: 5000.0,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                implementation: EqImplementation::LinearPhase { taps: 65 },
            },
        });
    }
    graph.nodes.push(GraphNode {
        id: 5,
        position: [0.0; 2],
        kind: GraphNodeKind::Denoise {
            config: rivu_dsp::denoise::DenoiseConfig {
                mode: rivu_dsp::denoise::DenoiseMode::Profile,
                ..Default::default()
            },
            profile: Some(NoiseSelection {
                start_seconds: 0.0,
                end_seconds: 0.04,
            }),
        },
    });
    graph.edges = [(1, 3), (3, 5), (5, 4), (4, 2)]
        .into_iter()
        .map(|(from, to)| GraphEdge {
            from: graph_port(from, 0),
            to: graph_port(to, 0),
        })
        .collect();
    Arc::make_mut(&mut core.state.system.config).processing_graph = graph;
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let offline = core.processing_status(1).unwrap();
    assert_eq!(
        offline.phase,
        ProcessingPhase::Complete,
        "{:?}",
        offline.error
    );
    let output = offline
        .graph_outputs
        .iter()
        .find(|o| o.port == graph_port(2, 0))
        .unwrap();
    let expected = hound::WavReader::open(&output.path)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(expected.len(), pcm.len());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Hybrid)
        .unwrap();
    complete(&mut core);
    let hybrid = core.processing_status(2).unwrap();
    assert_eq!(
        hybrid.phase,
        ProcessingPhase::Complete,
        "{:?}",
        hybrid.error
    );
    assert_eq!(hybrid.settings.graph_mode, Some(GraphExecutionMode::Hybrid));
    assert_eq!(hybrid.settings.graph_boundary, Some(graph_port(5, 0)));
    let context = hybrid
        .graph_outputs
        .iter()
        .find(|o| o.port == graph_port(5, 0))
        .unwrap()
        .context
        .as_ref()
        .expect("Hybrid prefix must retain FIR/denoise causal context");
    let context_path = context.path.clone();
    let plan_latency = hybrid.graph_plan.as_ref().unwrap().latency_frames;
    let runner = loop {
        if let AudioCommand::GraphAudition {
            path,
            expected_identity,
            runner,
            ..
        } = commands.recv_timeout(Duration::from_secs(2)).unwrap()
        {
            assert_eq!(path, context_path);
            assert_eq!(
                expected_identity,
                crate::processing::SourceStamp::read(&context_path)
                    .unwrap()
                    .identity()
            );
            assert_eq!(runner.published_frames(), Some((pcm.len() / 2) as u64));
            assert_eq!(runner.latency_frames() as u64, plan_latency);
            break runner;
        }
    };
    let cached = hound::WavReader::open(context_path)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    let actual = finished_graph_pcm(*runner, cached, pcm.len() / 2);
    assert_eq!(actual.len(), expected.len());
    for (index, (&actual, &expected)) in actual.iter().zip(&expected).enumerate() {
        assert!(
            (actual - expected).abs() < 2e-6,
            "sample={index}: hybrid={actual}, offline={expected}"
        );
    }
}

#[test]
fn graph_executes_shuffled_ancestors_once_with_independent_eq_and_unclipped_mix() {
    let (directory, mut core) = fixture();
    let expected = source(directory.path());
    let eq = |preamp_db| GraphNodeKind::Equalizer {
        config: rivu_dsp::equalizer::config::EqualizerConfig {
            enabled: true,
            preamp_db,
            ..Default::default()
        },
        implementation: EqImplementation::Iir,
    };
    let mut graph = ProcessingGraph {
        next_id: 7,
        ..Default::default()
    };
    graph.nodes.extend([
        GraphNode {
            id: 4,
            position: [0.0; 2],
            kind: eq(-6.020599913),
        },
        GraphNode {
            id: 3,
            position: [0.0; 2],
            kind: eq(6.020599913),
        },
        GraphNode {
            id: 5,
            position: [0.0; 2],
            kind: GraphNodeKind::Mix {
                gains_db: vec![0.0; 2],
                muted: vec![false; 2],
                solo: vec![false; 2],
            },
        },
        // This missing-model sibling must never execute or prevent ancestor preview.
        GraphNode {
            id: 6,
            position: [0.0; 2],
            kind: GraphNodeKind::Speech {
                config: Default::default(),
            },
        },
    ]);
    graph.edges = [
        (1, 0, 3, 0),
        (1, 0, 4, 0),
        (3, 0, 5, 0),
        (4, 0, 5, 1),
        (5, 0, 2, 0),
    ]
    .into_iter()
    .map(|(a, ap, b, bp)| GraphEdge {
        from: graph_port(a, ap),
        to: graph_port(b, bp),
    })
    .collect();
    Arc::make_mut(&mut core.state.system.config).processing_graph = graph.clone();
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    // The edit changes the draft while the admitted snapshot is immutable.
    core.processing_graph_edit(
        0,
        GraphEdit::Move {
            id: 3,
            position: [900.0, -80.0],
        },
    )
    .unwrap();
    assert!(graph.audio_eq(&core.state.system.config.processing_graph));
    complete(&mut core);
    let status = core.processing_status(1).unwrap();
    assert_eq!(
        status.phase,
        ProcessingPhase::Complete,
        "{:?}",
        status.error
    );
    assert_eq!(status.settings.graph, Some(graph));
    assert_eq!(
        status
            .graph_nodes
            .iter()
            .map(|n| n.node)
            .collect::<Vec<_>>(),
        vec![1, 3, 4, 5, 2]
    );
    assert!(
        status
            .graph_nodes
            .iter()
            .all(|n| n.phase == ProcessingPhase::Complete)
    );
    assert_eq!(status.graph_outputs.len(), 5);
    let output = &status
        .graph_outputs
        .iter()
        .find(|o| o.port == graph_port(2, 0))
        .unwrap()
        .path;
    let actual = hound::WavReader::open(output)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual.len(), expected.len());
    for (actual, source) in actual.iter().zip(expected) {
        assert!((*actual - source * 2.5).abs() < 2e-5);
    }
    assert!(actual.iter().any(|s| s.abs() > 1.0));
}

#[test]
fn graph_export_uses_frozen_graph_and_publishes_only_after_acceptance() {
    let (directory, mut core) = fixture();
    let expected = source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    core.processing_graph_edit(
        0,
        GraphEdit::Add {
            kind: GraphNodeKind::Denoise {
                config: Default::default(),
                profile: None,
            },
            position: [0.0; 2],
        },
    )
    .unwrap();
    let destination = directory.path().join("frozen graph export.wav");
    core.processing_graph_export(1, graph_port(2, 0), destination.clone())
        .unwrap();
    let event = completion(&mut core);
    assert!(!destination.exists());
    core.processing_event(event);
    let task = core.processing_status(2).unwrap();
    assert_eq!(task.phase, ProcessingPhase::Complete, "{:?}", task.error);
    assert_eq!(task.graph_exported_path, Some(destination.clone()));
    // A ready immutable output is copied, not needlessly decoded or run through DSP again.
    assert!(task.graph_nodes.is_empty());
    assert!(task.graph_outputs.is_empty());
    assert_eq!(task.settings.graph.unwrap().revision, 0);
    let actual = hound::WavReader::open(&destination)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
    assert!(
        core.processing_graph_export(1, graph_port(2, 0), destination)
            .is_err()
    );
}

#[test]
fn graph_exports_preserve_all_retained_runs_and_never_retain_export_scratch() {
    for cached in [true, false] {
        let (directory, mut core) = fixture();
        let expected = source(directory.path());
        for _ in 0..4 {
            core.processing_graph_run(
                1,
                graph_port(if cached { 2 } else { 1 }, 0),
                GraphExecutionMode::Offline,
            )
            .unwrap();
            complete(&mut core);
        }
        let outputs = core.processing_status(1).unwrap().graph_outputs;
        let mut action_run = core.processing_status(1).unwrap().graph_run_id.unwrap();
        for index in 0..6 {
            let destination = directory.path().join(format!("export-{index}.wav"));
            core.processing_graph_export(action_run, graph_port(2, 0), destination.clone())
                .unwrap();
            complete(&mut core);
            let task = core.processing_status(5 + index).unwrap();
            assert_eq!(task.phase, ProcessingPhase::Complete, "{:?}", task.error);
            assert_eq!(task.graph_exported_path, Some(destination.clone()));
            assert!(task.graph_outputs.is_empty());
            assert!(task.outputs.is_none());
            let actual = hound::WavReader::open(destination)
                .unwrap()
                .samples::<f32>()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(
                core.processing
                    .results
                    .iter()
                    .map(|r| r.status.id)
                    .collect::<Vec<_>>(),
                vec![1, 2, 3, 4]
            );
            for id in 1..=4 {
                let retained = core.processing_status(id).unwrap();
                assert!(retained.error.is_none());
                for output in retained.graph_outputs {
                    assert!(output.path.is_file());
                }
            }
            action_run = task.graph_run_id.unwrap();
        }
        assert_eq!(core.processing_status(1).unwrap().graph_outputs, outputs);
        let _commands = recording_audio(&mut core);
        core.processing_graph_audition(action_run, graph_port(if cached { 2 } else { 1 }, 0))
            .unwrap();

        // A destination created after preparation must survive the final
        // no-clobber commit, without evicting any audition output.
        let destination = directory.path().join("raced-export.wav");
        core.processing_graph_export(1, graph_port(2, 0), destination.clone())
            .unwrap();
        let event = completion(&mut core);
        std::fs::write(&destination, b"existing destination").unwrap();
        core.processing_event(event);
        assert_eq!(
            core.processing_status(11).unwrap().phase,
            ProcessingPhase::Failed
        );
        assert_eq!(std::fs::read(destination).unwrap(), b"existing destination");
        assert_eq!(core.processing_status(1).unwrap().graph_outputs, outputs);
        assert_eq!(core.processing.results.len(), 4);
    }
}

#[test]
fn graph_export_cancel_after_physical_completion_never_publishes_a_file() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let destination = directory.path().join("cancelled export.wav");
    core.processing_graph_export(1, graph_port(2, 0), destination.clone())
        .unwrap();
    let event = completion(&mut core);
    core.cancel_processing(2).unwrap();
    core.processing_event(event);
    assert_eq!(
        core.processing_status(2).unwrap().phase,
        ProcessingPhase::Cancelled
    );
    assert!(!destination.exists());
    assert!(
        core.processing_status(2)
            .unwrap()
            .graph_exported_path
            .is_none()
    );
}

#[test]
fn exporting_an_unrendered_port_executes_its_frozen_ancestor_closure() {
    let (directory, mut core) = fixture();
    let expected = source(directory.path());
    core.processing_graph_run(1, graph_port(1, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    assert_eq!(core.processing_status(1).unwrap().graph_outputs.len(), 1);
    let destination = directory.path().join("previously unrendered output.wav");
    core.processing_graph_export(1, graph_port(2, 0), destination.clone())
        .unwrap();
    complete(&mut core);
    let task = core.processing_status(2).unwrap();
    assert_eq!(task.phase, ProcessingPhase::Complete, "{:?}", task.error);
    assert_eq!(
        task.graph_nodes.iter().map(|n| n.node).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(task.graph_exported_path, Some(destination.clone()));
    let actual = hound::WavReader::open(destination)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn graph_export_source_change_at_publication_discards_staged_file() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let destination = directory.path().join("changed source export.wav");
    core.processing_graph_export(1, graph_port(2, 0), destination.clone())
        .unwrap();
    let event = completion(&mut core);
    std::fs::write(directory.path().join("1.wav"), b"changed source").unwrap();
    core.processing_event(event);
    assert_eq!(
        core.processing_status(2).unwrap().phase,
        ProcessingPhase::Failed
    );
    assert!(!destination.exists());
    assert!(
        core.processing_status(2)
            .unwrap()
            .graph_exported_path
            .is_none()
    );
}

#[test]
fn auto_graph_prepares_off_thread_and_opens_stopped_source_paused() {
    let (directory, mut core) = fixture();
    source(directory.path());
    let commands = recording_audio(&mut core);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Auto)
        .unwrap();
    assert_eq!(
        core.state.system.processing.as_ref().unwrap().phase,
        ProcessingPhase::Queued
    );
    assert!(core.state.processing_audition.is_none());
    complete(&mut core);
    let task = core.processing_status(1).unwrap();
    assert_eq!(task.phase, ProcessingPhase::Complete, "{:?}", task.error);
    assert_eq!(task.settings.graph_mode, Some(GraphExecutionMode::Realtime));
    assert!(task.graph_outputs.is_empty());
    assert_eq!(task.sample_rate, Some(48000));
    assert_eq!(task.channels, Some(2));
    assert_eq!(task.graph_plan.as_ref().unwrap().latency_frames, 0);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert_eq!(core.state.current_track.as_ref().unwrap().id, 1);
    assert_eq!(
        core.state.processing_audition,
        Some(crate::projection::ProcessingAudition {
            id: 1,
            output: Some(graph_port(2, 0)),
            mode: Some(GraphExecutionMode::Realtime),
        })
    );
    loop {
        let command = commands.recv_timeout(Duration::from_secs(2)).unwrap();
        if let AudioCommand::GraphAudition { paused, runner, .. } = command {
            assert!(paused);
            assert_eq!(runner.sample_rate(), 48000);
            assert_eq!(runner.channels(), 2);
            break;
        }
    }
    core.cancel_processing(1).unwrap();
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Cancelled
    );
    assert!(core.processing.audition.is_none());
    assert!(core.state.processing_audition.is_none());
    let cancelled = core.processing_status(1).unwrap();
    assert_eq!(core.processing_results[0], cancelled);
    assert_eq!(core.processing.results[0].status, cancelled);
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    assert_eq!(core.processing_status(1).unwrap(), cancelled);
    assert_eq!(terminal_wait(&mut core, 1), cancelled);
    complete(&mut core);
    core.processing_results.retain(|status| status.id != 1);
    assert_eq!(core.processing_status(1).unwrap(), cancelled);
    assert_eq!(terminal_wait(&mut core, 1), cancelled);
}

#[test]
fn cached_audition_tracks_the_selected_port_without_following_new_results() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    assert!(core.state.processing_audition.is_none());
    let _commands = recording_audio(&mut core);
    core.processing_graph_audition(1, graph_port(2, 0)).unwrap();
    let admitted = crate::projection::ProcessingAudition {
        id: 1,
        output: Some(graph_port(2, 0)),
        mode: Some(GraphExecutionMode::Offline),
    };
    assert_eq!(core.state.processing_audition, Some(admitted));
    core.publish();
    assert_eq!(
        crate::projection::GuiSnapshot::from_core(&core.shared.read())
            .system
            .processing_audition,
        Some(admitted)
    );
    assert!(
        core.processing_graph_audition(1, graph_port(2, 99))
            .is_err()
    );
    assert_eq!(core.state.processing_audition, Some(admitted));
    core.processing_graph_audition(1, graph_port(1, 0)).unwrap();
    let selected = crate::projection::ProcessingAudition {
        output: Some(graph_port(1, 0)),
        ..admitted
    };
    assert_eq!(core.state.processing_audition, Some(selected));
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    assert_eq!(core.state.system.processing.as_ref().unwrap().id, 2);
    assert_eq!(core.state.processing_audition, Some(selected));
    core.publish();
    assert_eq!(
        crate::projection::GuiSnapshot::from_core(&core.shared.read())
            .system
            .processing_audition,
        Some(selected)
    );
    core.stop().unwrap();
    assert!(core.state.processing_audition.is_none());
    core.publish();
    assert!(
        crate::projection::GuiSnapshot::from_core(&core.shared.read())
            .system
            .processing_audition
            .is_none()
    );
}

#[test]
fn newer_playback_generation_does_not_get_replaced_by_late_graph_preparation() {
    let (directory, mut core) = fixture();
    source(directory.path());
    let commands = recording_audio(&mut core);
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Auto)
        .unwrap();
    let event = completion(&mut core);
    core.generation = core.generation.wrapping_add(1);
    core.processing_event(event);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.processing.audition.is_none());
    assert!(
        !commands
            .try_iter()
            .any(|command| matches!(command, AudioCommand::GraphAudition { .. }))
    );
}

#[test]
fn generic_configure_cannot_overwrite_graph_from_a_stale_settings_snapshot() {
    let (_directory, mut core) = fixture();
    let mut stale = core.effective_config();
    core.processing_graph_edit(
        0,
        GraphEdit::Move {
            id: 1,
            position: [800.0, 500.0],
        },
    )
    .unwrap();
    stale.volume = 0.25;
    assert!(
        core.command(Command::Configure {
            config: Arc::new(stale)
        })
        .is_err()
    );
    assert_eq!(core.state.system.config.processing_graph.revision, 1);
    assert_eq!(
        core.state
            .system
            .config
            .processing_graph
            .node(1)
            .unwrap()
            .position,
        [800.0, 500.0]
    );
}

#[test]
fn selected_cache_budget_rejects_new_result_without_publishing_dangling_paths() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let retained = core.processing_status(1).unwrap().graph_outputs[0]
        .path
        .clone();
    // Exercise byte-budget admission without creating a multi-gigabyte test fixture.
    core.processing.results[0].cache_bytes = crate::processing::graph::MAX_RETAINED_CACHE_BYTES;
    core.processing.audition = Some(1);
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let rejected = core.processing_status(2).unwrap();
    assert_eq!(rejected.phase, ProcessingPhase::Failed);
    assert!(rejected.error.unwrap().contains("retention limit"));
    assert!(rejected.graph_outputs.is_empty());
    assert_eq!(core.processing.results.len(), 1);
    assert!(retained.exists());
}

#[test]
fn shutdown_closes_processing_delivery_and_retires_unpublished_outputs() {
    let (directory, mut core) = fixture();
    source(directory.path());
    for _ in 0..3 {
        core.start_processing(1, ProcessingOperation::Denoise, None)
            .unwrap();
        complete(&mut core);
    }
    let buffered = core.processing.results.pop_front().unwrap();
    let blocked = core.processing.results.pop_front().unwrap();
    let buffered_path = buffered.directory.path().to_path_buf();
    let blocked_path = blocked.directory.path().to_path_buf();
    let retained_path = core.processing.results[0].directory.path().to_path_buf();
    core.processing
        .sender
        .try_send(crate::processing::Event::Finished {
            id: buffered.status.id,
            result: Ok(Box::new(buffered)),
        })
        .ok()
        .unwrap();
    while core.processing.sender.len() < core.processing.sender.capacity().unwrap() {
        core.processing
            .sender
            .try_send(crate::processing::Event::Progress {
                id: 4,
                completed: 0,
                total: None,
                phase: ProcessingPhase::Processing,
            })
            .ok()
            .unwrap();
    }
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = cancelled.clone();
    let sender = core.processing.sender.clone();
    let (sent, delivery) = bounded(1);
    let (release, released) = bounded(1);
    let worker = thread::spawn(move || {
        let rejected = sender
            .send_blocking(crate::processing::Event::Finished {
                id: blocked.status.id,
                result: Ok(Box::new(blocked)),
            })
            .is_err();
        sent.send(rejected).unwrap();
        released.recv().unwrap();
    });
    core.processing.active = Some(crate::processing::Active {
        id: 4,
        cancelled,
        worker,
    });
    let (finished, completion) = bounded(1);
    let shutdown = thread::spawn(move || {
        core.processing.shutdown();
        assert!(core.processing.sender.is_closed());
        assert!(core.processing.receiver.try_recv().is_err());
        assert!(
            retained_path.exists(),
            "retained audition output retired too early"
        );
        finished.send(()).unwrap();
        core
    });
    let rejected = delivery.recv_timeout(Duration::from_secs(5)).unwrap();
    let waiting_for_retirement = completion.try_recv().is_err();
    let cancelled = observed.load(Ordering::Acquire);
    release.send(()).unwrap();
    let core = shutdown.join().unwrap();
    completion.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rejected, "shutdown accepted a new terminal delivery");
    assert!(cancelled);
    assert!(
        waiting_for_retirement,
        "shutdown returned before worker retirement"
    );
    assert!(!buffered_path.exists());
    assert!(!blocked_path.exists());
    assert!(core.processing.active.is_none());
}

#[test]
fn playback_bypass_preserves_parameters_queue_and_final_heard_snapshot() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let _initial_commands = recording_audio(&mut core);
    let equalizer = rivu_dsp::equalizer::config::EqualizerConfig {
        enabled: true,
        preamp_db: -4.0,
        bands: vec![rivu_dsp::equalizer::config::EqBand::default()],
        ..Default::default()
    };
    core.equalizer_edit(EqualizerEdit::Replace {
        config: equalizer.clone(),
    })
    .unwrap();
    core.audition_processing(1, AuditionMode::Denoised, 0.0, 0.0)
        .unwrap();
    core.state.playback.status = PlaybackStatus::Paused;
    core.playback.as_mut().unwrap().started = true;
    core.playback.as_mut().unwrap().heard = 2.5;
    core.playback.as_mut().unwrap().counted = true;
    let generation = core.generation;
    let _commands = recording_final_snapshot(
        &mut core,
        audio::AudioSnapshot {
            generation,
            position_seconds: 0.065,
            listened_seconds: 2.52,
        },
    );
    let queue = core.state.queue.clone();
    let track = core.state.current_track.clone();
    let denoise = core.state.system.config.denoise.clone();
    core.command(Command::ProcessingBypass).unwrap();
    let mut expected = equalizer;
    expected.enabled = false;
    assert_eq!(core.state.system.config.equalizer, expected);
    assert_eq!(core.state.system.config.denoise, denoise);
    assert_eq!(core.state.queue, queue);
    assert_eq!(core.state.current_track, track);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert_eq!(core.state.playback.position, 0.065);
    assert_eq!(core.playback.as_ref().unwrap().heard, 2.52);
    assert!(core.playback.as_ref().unwrap().counted);
    assert!(core.state.processing_audition.is_none());
    assert!(core.processing.audition.is_none());
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Complete
    );
    assert!(
        core.processing.results[0]
            .status
            .outputs
            .as_ref()
            .unwrap()
            .denoised
            .as_ref()
            .unwrap()
            .is_file()
    );
}

#[test]
fn playback_bypass_revokes_prepared_graph_selection_without_cancelling_job() {
    for mode in [GraphExecutionMode::Auto, GraphExecutionMode::Offline] {
        let (directory, mut core) = fixture();
        let expected = source(directory.path());
        let _commands = recording_audio(&mut core);
        core.processing_graph_run(1, graph_port(2, 0), mode)
            .unwrap();
        let event = completion(&mut core);
        let generation = core.generation;
        core.command(Command::ProcessingBypass).unwrap();
        assert!(
            !core
                .processing
                .active
                .as_ref()
                .unwrap()
                .cancelled
                .load(Ordering::Acquire)
        );
        core.processing_event(event);
        assert_eq!(
            core.processing_status(1).unwrap().phase,
            ProcessingPhase::Complete
        );
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.generation, generation);
        assert!(core.state.current_track.is_none());
        assert!(core.state.processing_audition.is_none());
        assert!(core.processing.pending_audition.is_none());
        if mode == GraphExecutionMode::Offline {
            let path = &core.processing.results[0].status.graph_outputs[0].path;
            let pcm = hound::WavReader::open(path)
                .unwrap()
                .samples::<f32>()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(pcm, expected);
        }
    }
}

#[test]
fn playback_bypass_revokes_late_mix_selection_but_keeps_completed_audio() {
    let (directory, mut core) = fixture();
    let original = source(directory.path());
    core.start_processing(1, ProcessingOperation::Denoise, None)
        .unwrap();
    complete(&mut core);
    let result = &mut core.processing.results[0];
    let outputs = result.status.outputs.as_mut().unwrap();
    outputs.vocals = outputs.denoised.clone();
    outputs.accompaniment = outputs.denoised.clone();
    let _commands = recording_audio(&mut core);
    core.audition_processing(1, AuditionMode::Mix, 0.0, 0.0)
        .unwrap();
    let event = next_processing_event(&mut core, Duration::from_secs(10)).unwrap();
    assert!(matches!(
        event,
        crate::processing::Event::MixFinished { result: Ok(_), .. }
    ));
    let generation = core.generation;
    core.command(Command::ProcessingBypass).unwrap();
    core.processing_event(event);
    assert_eq!(core.generation, generation);
    assert!(core.processing.audition.is_none());
    assert!(core.state.processing_audition.is_none());
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Complete
    );
    let pcm = hound::WavReader::open(core.processing.results[0].mix.as_ref().unwrap())
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        pcm,
        original
            .iter()
            .map(|sample| sample * 2.0)
            .collect::<Vec<_>>()
    );
}

#[test]
fn stale_queue_graph_audition_recovers_source_context_before_selecting_occurrence() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Complete
    );
    let connection = committed_queue_id_refresh_failure(&mut core, directory.path());
    let generation = core.generation;
    let _commands = recording_audio(&mut core);
    assert!(
        core.command(Command::ProcessingGraphAudition {
            id: 1,
            node: graph_port(2, 0)
        })
        .is_err()
    );
    assert_eq!(core.generation, generation);
    assert!(core.queue_ids_stale);
    repair_queue_ids(&connection);
    core.command(Command::ProcessingGraphAudition {
        id: 1,
        node: graph_port(2, 0),
    })
    .unwrap();
    assert!(!core.queue_ids_stale);
    assert_eq!(core.queue_ids, [1, 4, 5]);
    assert_eq!(core.state.queue.total, 3);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(core.state.current_track().unwrap().id, 1);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
}

#[test]
fn graph_exports_reject_every_owned_scratch_directory_and_alias() {
    let (directory, mut core) = fixture();
    source(directory.path());
    for _ in 0..3 {
        core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
            .unwrap();
        complete(&mut core);
    }
    let runs = core
        .processing
        .results
        .iter()
        .map(|result| result.directory.path().to_path_buf())
        .collect::<Vec<_>>();
    let scratch = runs[0].parent().unwrap().to_path_buf();
    for (index, run) in runs.iter().enumerate() {
        let nested = run.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let alias = directory.path().join(format!("scratch-alias-{index}"));
        std::os::unix::fs::symlink(&nested, &alias).unwrap();
        for parent in [run, &nested, &alias, &scratch] {
            let path = parent.join("must-not-export.wav");
            let error = core
                .processing_graph_export(1, graph_port(2, 0), path.clone())
                .unwrap_err();
            assert!(
                error.to_string().contains("automatically reclaimed"),
                "{error:#}"
            );
            assert!(!path.exists());
            assert!(core.processing.active.is_none());
        }
    }
    // Retirement does not make a session-owned location a durable destination.
    let retired = core.processing.results.pop_front().unwrap();
    core.processing.retire(Box::new(retired));
    let error = core
        .processing_graph_export(2, graph_port(2, 0), scratch.join("retiring.wav"))
        .unwrap_err();
    assert!(error.to_string().contains("automatically reclaimed"));
}

#[test]
fn graph_exports_reject_scratch_after_ancestor_rename_with_old_path_alias() {
    let (directory, mut core) = fixture();
    let expected = source(directory.path());
    let data = directory.path().join("data");
    core.processing = crate::processing::Service::new(data.clone());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let scratch = core.processing.results[0]
        .directory
        .path()
        .parent()
        .unwrap()
        .to_path_buf();
    let moved = directory.path().join("moved-data");
    std::fs::rename(&data, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &data).unwrap();
    let moved_scratch = moved.join(scratch.strip_prefix(&data).unwrap());
    for parent in [&scratch, &moved_scratch] {
        let destination = parent.join("must-not-export.wav");
        let error = core
            .processing_graph_export(1, graph_port(2, 0), destination.clone())
            .unwrap_err();
        assert!(
            error.to_string().contains("automatically reclaimed"),
            "{error:#}"
        );
        assert!(!destination.exists());
        assert!(core.processing.active.is_none());
    }
    let destination = directory.path().join("durable.wav");
    core.processing_graph_export(1, graph_port(2, 0), destination.clone())
        .unwrap();
    complete(&mut core);
    let status = core.processing_status(2).unwrap();
    assert_eq!(status.phase, ProcessingPhase::Complete);
    assert_eq!(status.graph_exported_path, Some(destination.clone()));
    drop(core);
    assert!(!moved_scratch.exists());
    let actual = hound::WavReader::open(destination)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn graph_exports_reject_unresolvable_owned_scratch_root() {
    let (directory, mut core) = fixture();
    source(directory.path());
    let data = directory.path().join("data");
    core.processing = crate::processing::Service::new(data.clone());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    std::fs::rename(&data, directory.path().join("moved-data")).unwrap();
    let destination = directory.path().join("durable.wav");
    let error = core
        .processing_graph_export(1, graph_port(2, 0), destination.clone())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Cannot resolve processing scratch root"),
        "{error:#}"
    );
    assert!(!destination.exists());
    assert!(core.processing.active.is_none());
}

#[test]
fn graph_export_freezes_external_alias_and_survives_processing_owner_drop() {
    let (directory, mut core) = fixture();
    let expected = source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let output = directory.path().join("durable");
    std::fs::create_dir(&output).unwrap();
    let alias = directory.path().join("destination-alias");
    std::os::unix::fs::symlink(&output, &alias).unwrap();
    core.processing_graph_export(1, graph_port(2, 0), alias.join("export.wav"))
        .unwrap();
    let event = completion(&mut core);
    let scratch = core.processing.results[0]
        .directory
        .path()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&scratch, &alias).unwrap();
    core.processing_event(event);
    let destination = output.join("export.wav");
    assert_eq!(
        core.processing_status(2).unwrap().graph_exported_path,
        Some(destination.clone())
    );
    assert!(!scratch.join("export.wav").exists());
    drop(core);
    assert!(!scratch.exists());
    let actual = hound::WavReader::open(destination)
        .unwrap()
        .samples::<f32>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn graph_export_rechecks_canonical_parent_before_publication() {
    let (directory, mut core) = fixture();
    source(directory.path());
    core.processing_graph_run(1, graph_port(2, 0), GraphExecutionMode::Offline)
        .unwrap();
    complete(&mut core);
    let parent = directory.path().join("export-parent");
    std::fs::create_dir(&parent).unwrap();
    core.processing_graph_export(1, graph_port(2, 0), parent.join("export.wav"))
        .unwrap();
    let event = completion(&mut core);
    let crate::processing::Event::Finished {
        result: Ok(result), ..
    } = &event
    else {
        panic!("expected staged export completion");
    };
    let scratch = result.directory.path().to_path_buf();
    std::fs::rename(&parent, directory.path().join("moved-parent")).unwrap();
    std::os::unix::fs::symlink(&scratch, &parent).unwrap();
    core.processing_event(event);
    let status = core.processing_status(2).unwrap();
    assert_eq!(status.phase, ProcessingPhase::Failed);
    assert!(status.graph_exported_path.is_none());
    assert!(!scratch.join("export.wav").exists());
}

fn processing_command_ack(core: &mut Core, command: Command) -> Ack {
    let (reply, response) = oneshot::channel();
    assert!(
        core.business_event(dispatch::BusinessEvent::Request(Request {
            operation: Operation::Command(command),
            reply: Some(reply),
        }))
    );
    let CoreResponse::Ack(ack) = futures::executor::block_on(response).unwrap() else {
        panic!("expected command acknowledgement");
    };
    assert!(ack.is_success(), "{ack:?}");
    ack
}

#[test]
fn processing_acks_identify_only_the_task_admitted_by_that_command() {
    let (directory, mut core) = fixture();
    source(directory.path());
    let _commands = recording_audio(&mut core);
    let run = || Command::ProcessingGraphRun {
        track_id: 1,
        target: graph_port(1, 0),
        mode: GraphExecutionMode::Offline,
    };
    assert_eq!(
        processing_command_ack(&mut core, run()).processing_id,
        Some(1)
    );
    complete(&mut core);
    assert_eq!(
        processing_command_ack(&mut core, run()).processing_id,
        Some(2)
    );
    let event = completion(&mut core);
    let cached = processing_command_ack(
        &mut core,
        Command::ProcessingGraphAudition {
            id: 1,
            node: graph_port(1, 0),
        },
    );
    assert_eq!(cached.processing_id, None);
    assert_eq!(core.processing.active.as_ref().unwrap().id, 2);
    core.processing_event(event);
    let uncached = processing_command_ack(
        &mut core,
        Command::ProcessingGraphAudition {
            id: 1,
            node: graph_port(2, 0),
        },
    );
    assert_eq!(uncached.processing_id, Some(3));
    core.processing_bypass().unwrap();
    complete(&mut core);
    let export = processing_command_ack(
        &mut core,
        Command::ProcessingGraphExport {
            id: 1,
            node: graph_port(1, 0),
            path: directory.path().join("ack-export.wav"),
        },
    );
    assert_eq!(export.processing_id, Some(4));
    complete(&mut core);
    let native = processing_command_ack(
        &mut core,
        Command::StartProcessing {
            track_id: 1,
            operation: ProcessingOperation::Denoise,
            profile: None,
        },
    );
    assert_eq!(native.processing_id, Some(5));
    complete(&mut core);
}

#[test]
fn playback_bypass_does_not_relax_mix_source_admission() {
    for change in ["file", "decoder", "database"] {
        let (directory, mut core) = fixture();
        source(directory.path());
        core.edit_track(1, None, None, None, Some(DecoderHint::Auto))
            .unwrap();
        prepare_mix(&mut core);
        let _commands = recording_audio(&mut core);
        core.audition_processing(1, AuditionMode::Mix, 0.0, 0.0)
            .unwrap();
        let event = mix_completion(&mut core);
        let crate::processing::Event::MixFinished {
            result: Ok(path), ..
        } = &event
        else {
            panic!("expected rendered mix");
        };
        let path = path.clone();
        core.processing_bypass().unwrap();
        match change {
            "file" => std::fs::write(directory.path().join("1.wav"), b"changed source").unwrap(),
            "decoder" => core
                .edit_track(1, None, None, None, Some(DecoderHint::Native))
                .unwrap(),
            "database" => core.database_outcome_uncertain(),
            _ => unreachable!(),
        }
        let generation = core.generation;
        core.processing_event(event);
        assert_eq!(
            core.processing_status(1).unwrap().phase,
            ProcessingPhase::Failed,
            "{change}"
        );
        assert!(core.processing.results[0].mix.is_none());
        assert!(!path.exists());
        assert!(core.processing.audition.is_none());
        assert_eq!(core.generation, generation);
    }
}

#[test]
fn quarantined_mix_completion_does_not_persist_pending_queue_selection() {
    let (directory, mut core) = fixture();
    source(directory.path());
    prepare_mix(&mut core);
    let commands = recording_audio(&mut core);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    let saved_current = core.store.queue_state().unwrap().current_id;
    connection
        .execute_batch(
            "CREATE TRIGGER reject_queue_selection BEFORE UPDATE ON rivu_meta
             BEGIN SELECT RAISE(ABORT, 'queue selection rejected'); END;",
        )
        .unwrap();
    assert!(core.command(Command::Play { track_id: 1 }).is_err());
    while !matches!(
        commands.recv_timeout(Duration::from_secs(10)).unwrap(),
        AudioCommand::Load { .. }
    ) {}
    assert!(core.queue_current_pending);
    connection
        .execute_batch("DROP TRIGGER reject_queue_selection")
        .unwrap();
    core.audition_processing(1, AuditionMode::Mix, 0.0, 0.0)
        .unwrap();
    let event = mix_completion(&mut core);
    let crate::processing::Event::MixFinished {
        result: Ok(path), ..
    } = &event
    else {
        panic!("expected rendered mix");
    };
    let path = path.clone();
    let generation = core.generation;
    let cache_bytes = core.processing.results[0].cache_bytes;
    core.database_outcome_uncertain();
    core.processing_event(event);
    assert_eq!(core.store.queue_state().unwrap().current_id, saved_current);
    assert!(core.queue_current_pending);
    assert_eq!(core.generation, generation);
    assert!(commands.try_recv().is_err());
    assert!(core.processing.audition.is_none());
    assert!(core.processing.results[0].mix.is_none());
    assert_eq!(core.processing.results[0].cache_bytes, cache_bytes);
    assert!(!path.exists());
    let status = core.processing_status(1).unwrap();
    assert_eq!(status.phase, ProcessingPhase::Failed);
    assert!(
        status
            .error
            .as_deref()
            .unwrap()
            .contains("Database outcome is uncertain")
    );
    assert_eq!(terminal_wait(&mut core, 1), status);
}

#[test]
fn processing_mix_ack_reports_admission_but_synchronous_audition_does_not() {
    let (directory, mut core) = fixture();
    source(directory.path());
    prepare_mix(&mut core);
    let _commands = recording_audio(&mut core);
    for (mode, expected) in [(AuditionMode::Original, None), (AuditionMode::Mix, Some(1))] {
        let ack = processing_command_ack(
            &mut core,
            Command::AuditionProcessing {
                id: 1,
                mode,
                vocal_gain_db: 0.0,
                accompaniment_gain_db: 0.0,
            },
        );
        assert_eq!(ack.processing_id, expected);
    }
    let event = mix_completion(&mut core);
    core.processing_bypass().unwrap();
    core.processing_event(event);
    assert_eq!(
        core.processing_status(1).unwrap().phase,
        ProcessingPhase::Complete
    );
}
