use super::*;

fn navigation_fixture(status: PlaybackStatus) -> (tempfile::TempDir, Core) {
    let (directory, mut core) = fixture();
    for id in 1..=4 {
        std::fs::write(directory.path().join(format!("{id}.wav")), []).unwrap();
    }
    let id = core.queue_ids[1];
    pending(&mut core, id);
    core.state.playback.status = status;
    if status == PlaybackStatus::Stopped {
        core.playback = None;
    }
    (directory, core)
}

fn assert_navigation_audio(commands: &Receiver<AudioCommand>, status: PlaybackStatus, path: &Path) {
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::StopAndSnapshot(_)
    ));
    if status != PlaybackStatus::Stopped {
        match commands.recv_timeout(Duration::from_secs(2)).unwrap() {
            AudioCommand::Load {
                path: loaded,
                paused,
                start_seconds,
                ..
            } => {
                assert_eq!(loaded, path);
                assert_eq!(paused, status == PlaybackStatus::Paused);
                assert_eq!(start_seconds, 0.0);
            }
            _ => panic!("navigation must load exactly once with the requested status"),
        }
    }
    assert!(commands.try_recv().is_err());
}

#[test]
fn navigation_endpoint_preserves_execution_status_and_occurrence_selection() {
    for status in [
        PlaybackStatus::Paused,
        PlaybackStatus::Stopped,
        PlaybackStatus::Playing,
    ] {
        for direction in [Navigation::Next, Navigation::Previous] {
            let (directory, mut core) = navigation_fixture(status);
            let expected = core.queue_ids[match direction {
                Navigation::Next => 2,
                Navigation::Previous => 0,
            }];
            let path = core
                .track(core.store.queue_track_id(expected).unwrap())
                .unwrap()
                .path;
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            let ack = futures::executor::block_on(handle.navigate(direction));
            assert!(ack.is_success(), "{:?}", ack.error);
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.playback.status, status);
            assert_eq!(snapshot.queue.current_id, Some(expected));
            assert_eq!(snapshot.playback.position, 0.0);
            assert_eq!(snapshot.current_track().unwrap().path, path);
            assert_eq!(snapshot.current_track().unwrap().play_count, 0);
            assert!(snapshot.current_track().unwrap().last_played.is_none());
            assert!(!publications.lock().is_empty());
            assert!(
                publications
                    .lock()
                    .iter()
                    .all(|state| state.playback.status == status)
            );
            assert_navigation_audio(&commands, status, &path);
            control.request_shutdown().unwrap();
            let core = worker.join().unwrap();
            assert_eq!(
                core.store
                    .queue_projection(core.queue_ids.len(), core.state.queue.revision)
                    .unwrap()
                    .current_id,
                Some(expected)
            );
            assert!(core.store.history(MAX_QUERY_ROWS).unwrap().is_empty());
            drop(directory);
        }
    }
}

#[test]
fn pause_accepted_before_queued_eof_keeps_next_and_repeat_one_paused() {
    for repeat in [RepeatMode::Off, RepeatMode::One] {
        let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
        core.state.playback.repeat = repeat;
        let expected = core.queue_ids[if repeat == RepeatMode::One { 1 } else { 2 }];
        let generation = core.generation;
        let commands = recording_audio(&mut core);
        core.command(Command::Pause).unwrap();
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        assert_eq!(core.state.queue.current_id, Some(expected));
        // Core sends Load synchronously, but the recorder forwards it on
        // another thread. Its snapshot reply follows recording every earlier
        // FIFO command, so draining cannot race the final paused Load.
        let (snapshot, recorded_through_snapshot) = oneshot::channel();
        core.engine
            .send(AudioCommand::StopAndSnapshot(snapshot))
            .unwrap();
        futures::executor::block_on(async {
            rivu_protocol::transport::timeout(Duration::from_secs(5), recorded_through_snapshot)
                .await
                .unwrap()
                .unwrap()
        });
        let loads: Vec<_> = commands
            .try_iter()
            .filter_map(|command| match command {
                AudioCommand::Load { paused, .. } => Some(paused),
                _ => None,
            })
            .collect();
        assert_eq!(loads, [true]);
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
        assert_eq!(core.state.queue.current_id, Some(expected));
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    }
}

#[test]
fn identical_shuffle_setter_preserves_previous_occurrence_history() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    core.state.playback.shuffle = true;
    let previous = core.queue_ids[3];
    let current = core.state.queue.current_id.unwrap();
    core.played = vec![previous, current];
    core.played_cursor = 2;
    let _commands = recording_audio(&mut core);
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.navigate(Navigation::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(previous));
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert_eq!(core.played, [previous, current]);
    assert_eq!(core.played_cursor, 1);
}

#[test]
fn seek_uses_explicit_cue_end_not_estimated_whole_track_duration() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    core.seek(25.0).unwrap();
    assert_eq!(core.state.playback.position, 25.0);
    Arc::make_mut(core.state.current_track.as_mut().unwrap()).cue = Some(CueSegment {
        sheet: "album.cue".into(),
        number: 1,
        start_frame: 75,
        end_frame: Some(300),
    });
    core.seek(25.0).unwrap();
    assert_eq!(core.state.playback.position, 3.0);
}

#[test]
fn queued_pause_is_authoritative_for_navigation_not_the_callers_snapshot() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
    let expected = core.queue_ids[2];
    let commands = recording_audio(&mut core);
    let (handle, control, worker, publications) = navigation_runtime(core);
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Playing
    );
    handle.send(Command::Pause).unwrap();
    assert!(futures::executor::block_on(handle.navigate(Navigation::Next)).is_success());
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Paused
    );
    assert_eq!(handle.mpris_snapshot().queue.current_id, Some(expected));
    assert!(
        publications
            .lock()
            .iter()
            .all(|state| state.playback.status == PlaybackStatus::Paused)
    );
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Pause(true)
    ));
    let path = handle
        .mpris_snapshot()
        .current_track()
        .unwrap()
        .path
        .clone();
    assert_navigation_audio(&commands, PlaybackStatus::Paused, &path);
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn previous_resets_position_without_loading_or_changing_status() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Playing] {
        let (_directory, mut core) = navigation_fixture(status);
        let current = core.state.queue.current_id;
        core.state.playback.position = 4.0;
        let seek_revision = core.state.playback.seek_revision;
        let commands = recording_audio(&mut core);
        let (handle, control, worker, publications) = navigation_runtime(core);
        assert!(futures::executor::block_on(handle.navigate(Navigation::Previous)).is_success());
        let snapshot = handle.mpris_snapshot();
        assert_eq!(snapshot.playback.status, status);
        assert_eq!(snapshot.queue.current_id, current);
        assert_eq!(snapshot.playback.position, 0.0);
        assert_eq!(
            snapshot.playback.seek_revision,
            seek_revision.wrapping_add(1)
        );
        assert!(
            publications
                .lock()
                .iter()
                .all(|state| state.playback.status == status)
        );
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Seek(0.0)
        ));
        assert!(commands.try_recv().is_err());
        control.request_shutdown().unwrap();
        worker.join().unwrap();
    }
}

#[test]
fn shuffle_previous_preserves_status_and_follows_occurrence_history() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
        let (_directory, mut core) = navigation_fixture(status);
        let ids = core.queue_ids.clone();
        pending(&mut core, ids[3]);
        core.state.playback.status = status;
        if status == PlaybackStatus::Stopped {
            core.playback = None;
        }
        core.state.playback.shuffle = true;
        core.played = vec![ids[0], ids[2], ids[3]];
        core.played_cursor = 3;
        let commands = recording_audio(&mut core);
        let (handle, control, worker, publications) = navigation_runtime(core);
        for expected in [ids[2], ids[0]] {
            assert!(
                futures::executor::block_on(handle.navigate(Navigation::Previous)).is_success()
            );
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.queue.current_id, Some(expected));
            assert_eq!(snapshot.playback.status, status);
            assert_navigation_audio(&commands, status, &snapshot.current_track().unwrap().path);
        }
        assert!(
            publications
                .lock()
                .iter()
                .all(|state| state.playback.status == status)
        );
        control.request_shutdown().unwrap();
        let core = worker.join().unwrap();
        assert_eq!(core.played, [ids[0], ids[2], ids[3]]);
        assert_eq!(core.played_cursor, 1);
    }
}

#[test]
fn existing_wire_navigation_still_starts_paused_and_stopped_tracks() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
        for command in [Command::Next, Command::Previous] {
            let (_directory, mut core) = navigation_fixture(status);
            let commands = recording_audio(&mut core);
            let (handle, control, worker, _) = navigation_runtime(core);
            assert!(futures::executor::block_on(handle.request_ack(command)).is_success());
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.playback.status, PlaybackStatus::Playing);
            assert_navigation_audio(
                &commands,
                PlaybackStatus::Playing,
                &snapshot.current_track().unwrap().path,
            );
            control.request_shutdown().unwrap();
            worker.join().unwrap();
        }
    }
}

#[test]
fn preserving_navigation_reuses_repeat_and_single_occurrence_shuffle_selection() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
        for (shuffle, repeat) in [
            (false, RepeatMode::All),
            (false, RepeatMode::One),
            (true, RepeatMode::Off),
        ] {
            let (_directory, mut core) = navigation_fixture(status);
            let expected = if shuffle {
                core.command(Command::ClearQueue).unwrap();
                core.enqueue(&[1]).unwrap();
                core.queue_ids[0]
            } else if repeat == RepeatMode::All {
                core.queue_ids[0]
            } else {
                // Manual Next ignores Repeat One, just like the wire command.
                core.queue_ids[2]
            };
            let current = if shuffle {
                expected
            } else if repeat == RepeatMode::All {
                *core.queue_ids.last().unwrap()
            } else {
                core.queue_ids[1]
            };
            pending(&mut core, current);
            core.state.playback.status = status;
            if status == PlaybackStatus::Stopped {
                core.playback = None;
            }
            core.state.playback.shuffle = shuffle;
            core.state.playback.repeat = repeat;
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            assert!(futures::executor::block_on(handle.navigate(Navigation::Next)).is_success());
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.queue.current_id, Some(expected));
            assert_eq!(snapshot.playback.status, status);
            assert!(
                publications
                    .lock()
                    .iter()
                    .all(|state| state.playback.status == status)
            );
            assert_navigation_audio(&commands, status, &snapshot.current_track().unwrap().path);
            control.request_shutdown().unwrap();
            worker.join().unwrap();
        }
    }
}

#[test]
fn stopped_shuffle_previous_without_history_at_first_occurrence_is_noop() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    let current = core.queue_ids[0];
    pending(&mut core, current);
    core.playback = None;
    core.state.playback.status = PlaybackStatus::Stopped;
    core.state.playback.shuffle = true;
    let commands = recording_audio(&mut core);
    let (handle, control, worker, _) = navigation_runtime(core);
    assert!(futures::executor::block_on(handle.navigate(Navigation::Previous)).is_success());
    assert_eq!(handle.mpris_snapshot().queue.current_id, Some(current));
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Stopped
    );
    assert!(commands.try_recv().is_err());
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn ended_missing_next_stops_and_resume_attempts_a_load() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.playback.position = 10.0;
    let generation = core.generation;
    let error = core
        .audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("2.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.playback.position, 0.0);
    assert!(core.playback.is_none());
    assert_eq!(core.store.history(1).unwrap()[0].track_id, 1);
    assert_ne!(core.generation, generation);
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 0.0);
    // Resume must try to reopen the selected track, not send Pause(false)
    // to an empty worker and falsely report playing.
    missing(&mut core, Command::Resume, 1);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
}

#[test]
fn missing_manual_next_preserves_current_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.playback.position = 4.0;
    let generation = core.generation;
    let played_at = core.store.history(1).unwrap()[0].played_at;
    missing(&mut core, Command::Next, 2);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(core.state.playback.position, 4.0);
    assert_eq!(core.generation, generation);
    assert_eq!(core.playback.as_ref().unwrap().track_id, 1);
    assert_eq!(core.store.history(1).unwrap()[0].played_at, played_at);
}

#[test]
fn repeat_one_only_repeats_natural_end_and_repeat_all_revisits_singleton() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.playback.repeat = RepeatMode::One;
    missing(&mut core, Command::Next, 2);
    let generation = core.generation;
    let error = core
        .audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("1.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    core.command(Command::RemoveTracks {
        track_ids: vec![2, 3, 4],
    })
    .unwrap();
    core.state.playback.shuffle = true;
    core.state.playback.repeat = RepeatMode::All;
    core.played = vec![1];
    core.played_cursor = 1;
    missing(&mut core, Command::Next, 1);
}

#[test]
fn random_draws_include_current_weight_duplicates_and_ignore_forward_history() {
    use rand::SeedableRng;
    let (_directory, mut core) = fixture();
    core.state.queue.current_id = Some(3);
    core.played = vec![1, 3, 5];
    core.played_cursor = 1;
    let mut rng = rand::rngs::StdRng::seed_from_u64(9);
    let mut counts = [0usize; 5];
    let mut immediate_repeat = false;
    let mut previous = 3;
    for _ in 0..30_000 {
        let id = core.random_queue_id(&mut rng).unwrap();
        immediate_repeat |= previous == id;
        counts[id as usize - 1] += 1;
        previous = id;
    }
    assert!(immediate_repeat);
    assert!(counts.iter().all(|count| (5_500..6_500).contains(count)));
    assert!(counts[1] + counts[2] > counts[0] * 18 / 10);
    core.command(Command::ClearQueue).unwrap();
    core.enqueue(&[1]).unwrap();
    let id = core.queue_ids[0];
    assert_eq!(core.random_queue_id(&mut rng), Some(id));
    assert_eq!(core.random_queue_id(&mut rng), Some(id));
    for _ in 0..5000 {
        core.record_queue_history(id);
    }
    assert_eq!(core.played.len(), 4096);
    assert_eq!(core.played_cursor, 4096);
}

#[test]
fn repeated_keyboard_intents_accumulate_from_one_snapshot_in_execution_order() {
    use crate::keyboard::{self, Action};

    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    core.state.playback.volume = 0.4;
    core.state.playback.repeat = RepeatMode::Off;
    core.state.playback.shuffle = false;
    core.state.playback.position = 2.0;
    let snapshot = core.state.playback.clone();
    let occurrence = core.state.queue.current_id;
    let input = |action| keyboard::playback_command(action, &snapshot, occurrence).unwrap();

    for _ in 0..2 {
        core.command(input(Action::VolumeUp)).unwrap();
        core.command(input(Action::SeekForward)).unwrap();
    }
    assert!((core.state.playback.volume - 0.5).abs() < 0.000001);
    assert_eq!(core.state.playback.position, 12.0);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    for _ in 0..2 {
        core.command(input(Action::VolumeDown)).unwrap();
        core.command(input(Action::SeekBackward)).unwrap();
    }
    assert!((core.state.playback.volume - 0.4).abs() < 0.000001);
    assert_eq!(core.state.playback.position, 2.0);
    for expected in [
        RepeatMode::All,
        RepeatMode::One,
        RepeatMode::Off,
        RepeatMode::All,
    ] {
        core.command(input(Action::Repeat)).unwrap();
        assert_eq!(core.state.playback.repeat, expected);
    }
    for expected in [true, false, true] {
        core.command(input(Action::Shuffle)).unwrap();
        assert_eq!(core.state.playback.shuffle, expected);
    }
    core.command(Command::Repeat {
        mode: RepeatMode::One,
    })
    .unwrap();
    core.command(input(Action::Repeat)).unwrap();
    assert_eq!(core.state.playback.repeat, RepeatMode::Off);
    core.command(Command::Shuffle { enabled: false }).unwrap();
    core.command(input(Action::Shuffle)).unwrap();
    assert!(core.state.playback.shuffle);
}

#[test]
fn relative_intents_clamp_owner_values_and_reject_nonfinite_deltas() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    let queue_id = core.state.queue.current_id.unwrap();
    for (volume, delta, expected) in [(0.98, 0.05, 1.0), (0.02, -0.05, 0.0)] {
        core.command(Command::Volume { value: volume }).unwrap();
        core.command(Command::AdjustVolume { delta }).unwrap();
        assert_eq!(core.state.playback.volume, expected);
    }
    core.state.playback.position = 2.0;
    core.command(Command::SeekRelative {
        queue_id,
        delta: -5.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 0.0);
    core.command(Command::SeekRelative {
        queue_id,
        delta: 25.0,
    })
    .unwrap();
    // Metadata duration is not a hard seek boundary for ordinary audio.
    assert_eq!(core.state.playback.position, 25.0);
    Arc::make_mut(core.state.current_track.as_mut().unwrap()).cue = Some(CueSegment {
        sheet: "album.cue".into(),
        number: 1,
        start_frame: 75,
        end_frame: Some(300),
    });
    core.command(Command::SeekRelative {
        queue_id,
        delta: 5.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 3.0);
    let revision = core.state.playback.seek_revision;
    for delta in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            core.command(Command::SeekRelative { queue_id, delta })
                .is_err()
        );
        assert_eq!(core.state.playback.position, 3.0);
        assert_eq!(core.state.playback.seek_revision, revision);
    }
    for delta in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(core.command(Command::AdjustVolume { delta }).is_err());
        assert_eq!(core.state.playback.volume, 0.0);
    }
}

#[test]
fn delayed_keyboard_seeks_cannot_target_next_duplicate_occurrence_or_stopped_playback() {
    use crate::keyboard::{self, Action};

    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    let snapshot = core.state.playback.clone();
    let old_occurrence = core.state.queue.current_id;
    let old_track = core.state.current_track().unwrap().id;
    let delayed: Vec<_> = [
        Action::SeekForward,
        Action::SeekBackward,
        Action::SeekStart,
        Action::SeekEnd,
    ]
    .into_iter()
    .map(|action| keyboard::playback_command(action, &snapshot, old_occurrence).unwrap())
    .collect();
    core.command(Command::Next).unwrap();
    assert_ne!(core.state.queue.current_id, old_occurrence);
    assert_eq!(core.state.current_track().unwrap().id, old_track);
    let revision = core.state.playback.seek_revision;
    for command in delayed {
        core.command(command).unwrap();
        assert_eq!(core.state.playback.position, 0.0);
        assert_eq!(core.state.playback.seek_revision, revision);
    }
    let queue_id = core.state.queue.current_id.unwrap();
    core.command(Command::Stop).unwrap();
    let revision = core.state.playback.seek_revision;
    core.command(Command::SeekRelative {
        queue_id,
        delta: 5.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.playback.position, 0.0);
    assert_eq!(core.state.playback.seek_revision, revision);
}
