use super::*;
use rivu_format::config::Config;

fn setup() -> (AnalysisEngine, AnalysisFrame, PcmState, DisplayDemand) {
    let settings = AnalysisSettings {
        fft_size: 512,
        fps: 60,
        ..AnalysisSettings::from(&Config::default())
    };
    let state = PcmState {
        active: true,
        sample_rate: 48_000,
        ..PcmState::default()
    };
    let demand = DisplayDemand {
        enabled: true,
        spectrum: true,
        visualizer: VisualizerDemand {
            stereo_density: true,
            pcm: true,
            spectrogram: true,
            ..VisualizerDemand::default()
        },
    };
    let mut engine = AnalysisEngine::new(settings);
    let mut frame = AnalysisFrame::default();
    engine.configure(settings, demand, state, 1, &mut frame);
    (engine, frame, state, demand)
}
fn header(state: PcmState, sequence: u64, start_frame: u64) -> PcmHeader {
    PcmHeader {
        sequence,
        start_frame,
        output_frame: start_frame,
        epoch: state.epoch,
        intent: state.intent,
        generation: state.generation,
        capture: state.capture,
        sample_rate: state.sample_rate,
        gap: false,
    }
}

#[test]
fn rejected_pcm_never_prepares_or_advances_analysis() {
    let (mut engine, mut frame, state, _) = setup();
    for field in 0..6 {
        let mut stale = header(state, 0, 0);
        match field {
            0 => stale.epoch += 2,
            1 => stale.intent += 1,
            2 => stale.generation += 1,
            3 => stale.capture += 1,
            4 => stale.sample_rate += 1,
            _ => stale.epoch += 1,
        }
        assert!(!engine.ingest(stale, [[0.5; 2]; 512].into_iter(), &mut frame));
        assert!(engine.analyzer.is_none());
        assert_eq!(engine.sample_time, Duration::ZERO);
    }
    engine.request_frame();
    assert!(!engine.publish(&mut frame, Instant::now()));
    let pending = PcmState { intent: 1, ..state };
    engine.configure(engine.settings, engine.demand, pending, 2, &mut frame);
    assert!(!engine.ingest(
        header(pending, 0, 0),
        [[0.5; 2]; 512].into_iter(),
        &mut frame
    ));
}

#[test]
fn growing_logical_block_accepts_only_its_new_contiguous_tail() {
    let (mut engine, mut frame, state, _) = setup();
    let first = header(state, 7, 0);
    assert!(engine.ingest(first, [[0.25; 2]; 256].into_iter(), &mut frame));
    let generation = frame.generation;
    let sample_time = engine.sample_time;
    assert!(!engine.ingest(first, [[1.0; 2]; 256].into_iter(), &mut frame));
    assert!(!engine.ingest(
        header(state, 6, 256),
        [[1.0; 2]; 256].into_iter(),
        &mut frame
    ));
    assert_eq!(frame.generation, generation);
    assert_eq!(engine.sample_time, sample_time);
    assert!(engine.ingest(
        header(state, 7, 256),
        [[0.75; 2]; 256].into_iter(),
        &mut frame
    ));
    engine.request_frame();
    assert!(engine.publish(&mut frame, Instant::now()));
    assert!((frame.rms_left - 0.3125_f32.sqrt()).abs() < 1e-6);
    assert_eq!(&frame.visualizer.pcm[..256], &[[0.25; 2]; 256]);
    assert_eq!(&frame.visualizer.pcm[256..], &[[0.75; 2]; 256]);
    assert_eq!(frame.generation, generation);
    assert!(engine.ingest(
        header(state, 8, 512),
        [[0.5; 2]; 512].into_iter(),
        &mut frame
    ));
    assert_eq!(frame.generation, generation);
}

#[test]
fn invalid_sample_observation_revokes_history_without_committing_audio_time() {
    let (mut engine, mut frame, state, _) = setup();
    let now = Instant::now();
    assert!(engine.ingest(
        header(state, 1, 0),
        [[0.25; 2]; 512].into_iter(),
        &mut frame
    ));
    engine.request_frame();
    assert!(engine.publish(&mut frame, now));
    let previous = frame.clone();
    assert!(!engine.ingest_checked(
        header(state, 2, 512),
        [[1.0, -1.0]; 256].into_iter(),
        || false,
        &mut frame,
    ));
    assert!(frame.generation > previous.generation);
    assert_eq!(frame.sample_time, previous.sample_time);
    assert_eq!(engine.sample_time, previous.sample_time);
    assert!(frame.settings.is_none());
    assert!(frame.visualizer.density.is_empty());
    assert!(frame.visualizer.pcm.is_empty());
    assert!(frame.spectrum_db.is_empty());
    assert!(engine.expected.is_none());
    assert!(!engine.fresh);
    assert!(!engine.requested);
    engine.request_frame();
    assert!(!engine.publish(&mut frame, now + Duration::from_secs(1)));

    // Retry the same logical position. Neither the failed span nor earlier
    // density history may contribute to the clean replacement window.
    assert!(engine.ingest_checked(
        header(state, 2, 512),
        [[0.75; 2]; 256].into_iter(),
        || true,
        &mut frame,
    ));
    assert!(!engine.publish(&mut frame, now + Duration::from_secs(1)));
    assert!(engine.ingest_checked(
        header(state, 2, 768),
        [[0.75; 2]; 256].into_iter(),
        || true,
        &mut frame,
    ));
    engine.request_frame();
    assert!(engine.publish(&mut frame, now + Duration::from_secs(1)));
    let (mut reference, mut expected, reference_state, _) = setup();
    assert!(reference.ingest(
        header(reference_state, 2, 512),
        [[0.75; 2]; 512].into_iter(),
        &mut expected,
    ));
    reference.request_frame();
    assert!(reference.publish(&mut expected, now));
    assert_eq!(frame.spectrum_db, expected.spectrum_db);
    assert_eq!(frame.visualizer.density, expected.visualizer.density);
    assert_eq!(frame.visualizer.pcm, expected.visualizer.pcm);
    assert_eq!(frame.rms_left, expected.rms_left);
    assert_eq!(
        frame.sample_time,
        previous.sample_time + Duration::from_secs_f64(256.0 / 48_000.0) * 2
    );
}

#[test]
fn permit_survives_partial_windows_and_fps_only_changes() {
    let (mut engine, mut frame, state, demand) = setup();
    engine.request_frame();
    engine.ingest(
        header(state, 0, 0),
        [[0.5; 2]; 256].iter().copied(),
        &mut frame,
    );
    assert_eq!(engine.publication_delay(Instant::now()), None);
    let generation = frame.generation;
    let settings = AnalysisSettings {
        fps: 5,
        ..engine.settings
    };
    engine.configure(settings, demand, state, 1, &mut frame);
    engine.ingest(
        header(state, 1, 256),
        [[0.5; 2]; 256].iter().copied(),
        &mut frame,
    );
    assert!(engine.publish(&mut frame, Instant::now()));
    assert_eq!(frame.generation, generation);
    assert_eq!(frame.rms_left, 0.5);
    assert!(!engine.publish(&mut frame, Instant::now() + Duration::from_secs(1)));
}

#[test]
fn publication_is_coalesced_capped_and_requires_fresh_input() {
    let (mut engine, mut frame, state, _) = setup();
    let now = Instant::now();
    engine.ingest(
        header(state, 0, 0),
        [[0.5; 2]; 512].iter().copied(),
        &mut frame,
    );
    assert!(!engine.publish(&mut frame, now));
    engine.request_frame();
    engine.request_frame();
    assert!(engine.publish(&mut frame, now));
    engine.request_frame();
    assert_eq!(engine.publication_delay(now), None);
    engine.ingest(
        header(state, 1, 512),
        [[0.5; 2]; 512].iter().copied(),
        &mut frame,
    );
    assert!(!engine.publish(&mut frame, now));
    assert!(engine.publication_delay(now).unwrap() > Duration::ZERO);
    assert!(engine.publish(&mut frame, now + Duration::from_secs(1)));
}

#[test]
fn capture_restart_clears_windows_but_pause_preserves_continuity() {
    let (mut engine, mut frame, state, demand) = setup();
    engine.ingest(
        header(state, 0, 0),
        [[0.5; 2]; 512].iter().copied(),
        &mut frame,
    );
    engine.request_frame();
    assert!(engine.publish(&mut frame, Instant::now()));
    let retained = frame.visualizer.pcm.clone();
    let generation = frame.generation;
    let paused = PcmState {
        epoch: 2,
        capture: 1,
        active: false,
        ..state
    };
    engine.configure(engine.settings, demand, paused, 2, &mut frame);
    assert_eq!(frame.generation, generation);
    assert!(frame.settings.is_none());
    let resumed = PcmState {
        epoch: 4,
        capture: 2,
        ..state
    };
    engine.configure(engine.settings, demand, resumed, 3, &mut frame);
    assert!(!engine.ingest(
        header(state, 1, 512),
        [[1.0; 2]; 512].into_iter(),
        &mut frame
    ));
    engine.request_frame();
    engine.ingest(
        header(resumed, 2, 1024),
        [[0.0; 2]; 256].iter().copied(),
        &mut frame,
    );
    assert!(!engine.publish(&mut frame, Instant::now() + Duration::from_secs(1)));
    engine.ingest(
        header(resumed, 3, 1280),
        [[0.0; 2]; 256].iter().copied(),
        &mut frame,
    );
    assert!(engine.publish(&mut frame, Instant::now() + Duration::from_secs(1)));
    assert_eq!(frame.generation, generation);
    assert_eq!(frame.rms_left, 0.0);
    assert!(retained.iter().all(|sample| *sample == [0.5; 2]));
    let stopped = PcmState {
        active: false,
        generation: 1,
        epoch: 6,
        ..resumed
    };
    engine.configure(engine.settings, demand, stopped, 4, &mut frame);
    assert_ne!(frame.generation, generation);
}

#[test]
fn sequence_start_frame_and_explicit_gaps_require_complete_new_windows() {
    for kind in 0..3 {
        let (mut engine, mut frame, state, _) = setup();
        engine.ingest(
            header(state, 0, 0),
            [[1.0; 2]; 512].iter().copied(),
            &mut frame,
        );
        let generation = frame.generation;
        let mut next = header(state, 1, 512);
        match kind {
            0 => next.sequence += 1,
            1 => next.start_frame += 1,
            _ => next.gap = true,
        }
        engine.request_frame();
        engine.ingest(next, [[0.0; 2]; 256].iter().copied(), &mut frame);
        assert_ne!(frame.generation, generation);
        assert!(frame.settings.is_none());
        assert!(!engine.publish(&mut frame, Instant::now()));
        let next = header(state, next.sequence + 1, next.start_frame + 256);
        engine.ingest(next, [[0.0; 2]; 256].iter().copied(), &mut frame);
        assert!(engine.publish(&mut frame, Instant::now()));
        assert_eq!(frame.rms_left, 0.0);
    }
}

#[test]
fn plot_only_changes_preserve_partial_fft_but_retire_old_publications() {
    let (mut engine, mut frame, state, demand) = setup();
    engine.ingest(
        header(state, 0, 0),
        [[0.5; 2]; 256].iter().copied(),
        &mut frame,
    );
    let changed = DisplayDemand {
        visualizer: VisualizerDemand::default(),
        ..demand
    };
    engine.configure(engine.settings, changed, state, 2, &mut frame);
    assert!(frame.settings.is_none());
    engine.ingest(
        header(state, 1, 256),
        [[0.5; 2]; 256].iter().copied(),
        &mut frame,
    );
    engine.request_frame();
    assert!(engine.publish(&mut frame, Instant::now()));
    assert_eq!(frame.rms_left, 0.5);
    assert!(frame.visualizer.density.is_empty());
}

#[test]
fn incompatible_configuration_discards_partial_samples() {
    for kind in 0..3 {
        let (mut engine, mut frame, state, demand) = setup();
        engine.ingest(
            header(state, 0, 0),
            [[1.0; 2]; 256].iter().copied(),
            &mut frame,
        );
        let mut settings = engine.settings;
        match kind {
            0 => settings.fft_size = 1024,
            1 => settings.window = AnalysisWindow::None,
            _ => settings.signal = rivu_format::config::VisualizationSignal::Before,
        }
        // Ensure each selected setting actually changes the default.
        if settings.signal == engine.settings.signal && kind == 2 {
            settings.signal = rivu_format::config::VisualizationSignal::After;
        }
        if settings.window == engine.settings.window && kind == 1 {
            settings.window = AnalysisWindow::BlackmanHarris;
        }
        let generation = frame.generation;
        engine.configure(settings, demand, state, 2, &mut frame);
        assert_ne!(frame.generation, generation);
        engine.request_frame();
        engine.ingest(
            header(state, 1, 256),
            [[0.0; 2]; 256].iter().copied(),
            &mut frame,
        );
        assert!(!engine.publish(&mut frame, Instant::now()));
        engine.ingest(
            header(state, 2, 512),
            vec![[0.0; 2]; settings.fft_size as usize - 256]
                .iter()
                .copied(),
            &mut frame,
        );
        assert!(engine.publish(&mut frame, Instant::now()));
        assert_eq!(frame.rms_left, 0.0);
    }
}

#[test]
fn density_ingests_the_entire_stream_not_only_latest_fft_tail() {
    let (mut engine, mut frame, state, _) = setup();
    for block in 0..64 {
        let sample = if block == 0 { [0.7, -0.7] } else { [0.0; 2] };
        engine.ingest(
            header(state, block, block * 512),
            [sample; 512].iter().copied(),
            &mut frame,
        );
    }
    engine.request_frame();
    assert!(engine.publish(&mut frame, Instant::now()));
    assert_eq!(frame.rms_left, 0.0);
    assert!(frame.visualizer.density.iter().any(|value| *value > 0.0));
    assert!(
        frame
            .sample_time
            .abs_diff(Duration::from_secs_f64(32768.0 / 48000.0))
            < Duration::from_nanos(64)
    );
}

#[test]
fn time_domain_demand_skips_fft_without_shortening_window_or_cadence() {
    for visualizer in [
        VisualizerDemand::default(),
        VisualizerDemand {
            pcm: true,
            ..Default::default()
        },
        VisualizerDemand {
            stereo_density: true,
            ..Default::default()
        },
        VisualizerDemand {
            pcm: true,
            stereo_density: true,
            ..Default::default()
        },
    ] {
        let (mut engine, mut frame, state, _) = setup();
        let demand = DisplayDemand {
            enabled: true,
            spectrum: false,
            visualizer,
        };
        engine.configure(engine.settings, demand, state, 2, &mut frame);
        engine.request_frame();
        engine.ingest(
            header(state, 0, 0),
            [[0.5, -0.25]; 511].into_iter(),
            &mut frame,
        );
        let untouched = Complex32::new(123.0, -456.0);
        engine.analyzer.as_mut().unwrap().frequency.fill(untouched);
        let now = Instant::now();
        assert_eq!(engine.publication_delay(now), None);
        assert!(!engine.publish(&mut frame, now));
        assert!(frame.settings.is_none());
        engine.ingest(
            header(state, 1, 511),
            [[0.5, -0.25]].into_iter(),
            &mut frame,
        );
        assert!(engine.publish(&mut frame, now));
        assert!(frame.settings.is_some());
        assert_eq!([frame.rms_left, frame.rms_right], [0.5, 0.25]);
        assert_eq!(
            frame.sample_time,
            Duration::from_secs_f64(511.0 / 48_000.0) + Duration::from_secs_f64(1.0 / 48_000.0)
        );
        assert!(frame.frequencies_hz.is_empty());
        assert!(frame.spectrum_db.is_empty());
        assert_eq!(
            frame.visualizer.pcm.len(),
            if visualizer.pcm { 512 } else { 0 }
        );
        if visualizer.pcm {
            assert!(
                frame
                    .visualizer
                    .pcm
                    .iter()
                    .all(|samples| *samples == [0.5, -0.25])
            );
        }
        if visualizer.stereo_density {
            assert!(frame.visualizer.density.iter().any(|value| *value > 0.0));
            assert_eq!(frame.visualizer.correlation, Some(-1.0));
            assert_eq!(frame.visualizer.balance, Some(-0.6));
            assert_eq!(frame.visualizer.side_ratio, Some(0.9));
        }
        engine.request_frame();
        assert_eq!(engine.publication_delay(now), None);
        engine.ingest(
            header(state, 2, 512),
            [[0.5, -0.25]].into_iter(),
            &mut frame,
        );
        assert!(!engine.publish(&mut frame, now));
        let interval = Duration::from_secs_f64(1.0 / engine.settings.fps as f64);
        assert_eq!(engine.publication_delay(now), Some(interval));
        assert!(engine.publish(&mut frame, now + interval));
        assert!(
            engine
                .analyzer
                .as_ref()
                .unwrap()
                .frequency
                .iter()
                .all(|bin| *bin == untouched)
        );
    }
}

#[test]
fn frequency_demand_transitions_preserve_partial_window_and_restore_outputs() {
    let (mut engine, mut frame, state, _) = setup();
    let time_domain = DisplayDemand {
        enabled: true,
        spectrum: false,
        visualizer: VisualizerDemand {
            pcm: true,
            stereo_density: true,
            ..Default::default()
        },
    };
    let demands = [
        DisplayDemand {
            spectrum: true,
            ..time_domain
        },
        DisplayDemand {
            visualizer: VisualizerDemand {
                spatial_spectrum: true,
                ..time_domain.visualizer
            },
            ..time_domain
        },
        DisplayDemand {
            visualizer: VisualizerDemand {
                band_energy: true,
                ..time_domain.visualizer
            },
            ..time_domain
        },
        DisplayDemand {
            visualizer: VisualizerDemand {
                spectrogram: true,
                ..time_domain.visualizer
            },
            ..time_domain
        },
        DisplayDemand {
            spectrum: true,
            ..time_domain
        },
    ];
    let now = Instant::now();
    let mut expected_fft = None;
    for (index, demand) in demands.into_iter().enumerate() {
        let token = index as u64 * 2 + 2;
        let start = index as u64 * 512;
        engine.configure(engine.settings, time_domain, state, token, &mut frame);
        let generation = frame.generation;
        engine.ingest(
            header(state, index as u64 * 2, start),
            [[0.5, -0.25]; 256].into_iter(),
            &mut frame,
        );
        engine.request_frame();
        let at = now + Duration::from_secs(index as u64 * 2);
        if index > 0 {
            assert!(engine.publish(&mut frame, at));
            assert_eq!(frame.frequencies_hz.capacity(), 0);
            assert_eq!(frame.spectrum_db.capacity(), 0);
        }
        engine.configure(engine.settings, demand, state, token + 1, &mut frame);
        assert!(frame.settings.is_none());
        engine.ingest(
            header(state, index as u64 * 2 + 1, start + 256),
            [[0.5, -0.25]; 256].into_iter(),
            &mut frame,
        );
        engine.request_frame();
        assert!(engine.publish(&mut frame, at + Duration::from_secs(1)));
        assert_eq!(frame.generation, generation);
        assert_eq!([frame.rms_left, frame.rms_right], [0.5, 0.25]);
        assert_eq!(frame.spectrum_requested, demand.spectrum);
        assert_eq!(frame.frequencies_hz.is_empty(), !demand.spectrum);
        assert_eq!(frame.spectrum_db.is_empty(), !demand.spectrum);
        assert_eq!(
            frame.visualizer.spatial.len(),
            if demand.visualizer.spatial_spectrum {
                256
            } else {
                0
            }
        );
        assert_eq!(
            frame.visualizer.spectrogram.len(),
            if demand.visualizer.spectrogram {
                256
            } else {
                0
            }
        );
        if demand.visualizer.band_energy {
            assert!(frame.visualizer.band_rms.iter().any(|rms| *rms > 0.0));
        }
        let frequency = &engine.analyzer.as_ref().unwrap().frequency;
        if let Some(expected) = &expected_fft {
            assert_eq!(frequency, expected);
        } else {
            expected_fft = Some(frequency.clone());
        }
    }
}
