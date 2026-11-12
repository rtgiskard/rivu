use super::*;

fn navigation_fixture(status: PlaybackStatus) -> (tempfile::TempDir, Core) {
    let (directory, mut core) = fixture();
    for id in 1..=4 {
        std::fs::write(directory.path().join(format!("{id}.wav")), []).unwrap();
    }
    let id = core.store.queue_id_at(1).unwrap().unwrap();
    pending(&mut core, id);
    core.state.playback.status = status;
    if status == PlaybackStatus::Stopped {
        core.transport.session = None;
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
fn navigation_commands_preserve_execution_status_and_occurrence_selection() {
    for status in [
        PlaybackStatus::Paused,
        PlaybackStatus::Stopped,
        PlaybackStatus::Playing,
    ] {
        for command in [Command::Next, Command::Previous] {
            let (directory, mut core) = navigation_fixture(status);
            core.state.playback.position = 4.0;
            let expected = core
                .store
                .queue_id_at(if matches!(command, Command::Next) {
                    2
                } else {
                    0
                })
                .unwrap()
                .unwrap();
            let path = core
                .track(core.store.queue_track_id(expected).unwrap())
                .unwrap()
                .path;
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            let ack = futures::executor::block_on(handle.request_ack(command));
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
                    .queue_projection(core.state.queue.revision)
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
        edit_preferences(&mut core, |config| config.repeat = repeat);
        let expected = core
            .store
            .queue_id_at(if repeat == RepeatMode::One { 1 } else { 2 })
            .unwrap()
            .unwrap();
        let generation = core.transport.generation;
        let commands = recording_audio(&mut core);
        core.command(Command::Pause).unwrap();
        core.audio_event(AudioEvent::Ended {
            generation,
            completed_duration: None,
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
            completed_duration: None,
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
    edit_preferences(&mut core, |config| config.shuffle = true);
    let previous = core.store.queue_id_at(3).unwrap().unwrap();
    let current = core.state.queue.current_id.unwrap();
    core.transport.history = vec![previous, current];
    core.transport.history_cursor = 2;
    let _commands = recording_audio(&mut core);
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(previous));
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert_eq!(core.transport.history, [previous, current]);
    assert_eq!(core.transport.history_cursor, 1);
}

fn start_cue_navigation(core: &mut Core) -> u64 {
    let path = core.state.current_track().unwrap().path.clone();
    let track_id = register_cue(
        core,
        &path,
        CueSegment {
            sheet: path.with_extension("cue"),
            number: 1,
            start_frame: 75,
            end_frame: Some(300),
        },
        "Cue segment",
    );
    core.enqueue(&[track_id]).unwrap();
    let id = core
        .store
        .queue_id_at(core.state.queue.total - 1)
        .unwrap()
        .unwrap();
    core.start_with_status(id, QueueHistory::Preserve, PlaybackStatus::Paused)
        .unwrap();
    id
}

#[test]
fn seek_uses_explicit_cue_end_not_estimated_whole_track_duration() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    core.seek(25.0).unwrap();
    assert_eq!(core.state.playback.position, 25.0);
    start_cue_navigation(&mut core);
    core.seek(25.0).unwrap();
    assert_eq!(core.state.playback.position, 3.0);
}

#[test]
fn invalid_seek_ack_preserves_paused_playback_and_valid_seek_stays_paused() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    core.state.playback.position = 0.25;
    let revision = core.state.playback.seek_revision;
    let commands = recording_audio(&mut core);
    let (handle, control, worker, _) = navigation_runtime(core);
    for seconds in [f64::NAN, f64::INFINITY, -1.0, 1e300, i64::MAX as f64] {
        let ack = futures::executor::block_on(handle.request_ack(Command::Seek { seconds }));
        assert!(!ack.is_success());
        let snapshot = handle.mpris_snapshot();
        assert_eq!(snapshot.playback.status, PlaybackStatus::Paused);
        assert_eq!(snapshot.playback.position, 0.25);
        assert_eq!(snapshot.playback.seek_revision, revision);
        assert!(commands.try_recv().is_err());
    }
    let ack = futures::executor::block_on(handle.request_ack(Command::Seek { seconds: 0.5 }));
    assert!(ack.is_success());
    let snapshot = handle.mpris_snapshot();
    assert_eq!(snapshot.playback.status, PlaybackStatus::Paused);
    assert_eq!(snapshot.playback.position, 0.5);
    assert_eq!(snapshot.playback.seek_revision, revision.wrapping_add(1));
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Seek { seconds: 0.5, revision: actual } if actual == revision.wrapping_add(1)
    ));
    assert!(commands.try_recv().is_err());
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn backend_seek_rejection_restores_paused_position_without_stopping() {
    for opened in [false, true] {
        let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
        let commands = recording_audio(&mut core);
        if opened {
            started(&mut core, Some(20.0)).unwrap();
        }
        core.state.playback.position = 0.25;
        core.command(Command::Seek { seconds: 1e16 }).unwrap();
        let revision = core.state.playback.seek_revision;
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Seek { seconds: 1e16, revision: actual } if actual == revision
        ));
        assert_eq!(core.state.playback.position, 1e16);
        core.audio_event(AudioEvent::SeekRejected {
            revision,
            snapshot: audio::AudioSnapshot {
                generation: core.transport.generation,
                position_seconds: 0.25,
                listened_seconds: 0.0,
            },
            message: "Target exceeds the opened backend timebase".into(),
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        assert_eq!(core.state.playback.position, 0.25);
        assert_eq!(core.state.playback.seek_revision, revision.wrapping_add(1));
        assert_eq!(
            core.state.system.last_error.as_deref(),
            Some("Target exceeds the opened backend timebase")
        );
        assert!(core.transport.session.is_some());
        assert!(
            commands.try_recv().is_err(),
            "rejection must not stop output"
        );
    }
}

#[test]
fn rejected_seek_progress_uses_settled_intent_not_published_clock_revision() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    started(&mut core, Some(20.0)).unwrap();
    core.command(Command::Seek { seconds: 1e16 }).unwrap();
    let revision = core.state.playback.seek_revision;
    core.audio_event(AudioEvent::SeekRejected {
        revision,
        snapshot: audio::AudioSnapshot {
            generation: core.transport.generation,
            position_seconds: 0.25,
            listened_seconds: 0.5,
        },
        message: "Target exceeds the opened backend timebase".into(),
    })
    .unwrap();
    let correction_revision = core.state.playback.seek_revision;
    assert_ne!(correction_revision, revision);
    core.audio_event(AudioEvent::Progress {
        generation: core.transport.generation,
        seek_revision: 0,
        position_seconds: 0.125,
        listened_seconds: 0.75,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 0.25);
    assert_eq!(core.transport.session.as_ref().unwrap().heard, 0.75);
    core.audio_event(AudioEvent::Progress {
        generation: core.transport.generation,
        seek_revision: revision,
        position_seconds: 0.5,
        listened_seconds: 1.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 0.5);
    assert_eq!(core.transport.session.as_ref().unwrap().heard, 1.0);
    assert_eq!(core.state.playback.seek_revision, correction_revision);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
}

#[test]
fn old_seek_rejections_cannot_roll_back_newer_seek_or_generation() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let commands = recording_audio(&mut core);
    core.command(Command::Seek { seconds: 1e16 }).unwrap();
    let rejected_revision = core.state.playback.seek_revision;
    core.command(Command::Seek { seconds: 0.5 }).unwrap();
    let current_revision = core.state.playback.seek_revision;
    for (generation, revision) in [
        (core.transport.generation, rejected_revision),
        (core.transport.generation.wrapping_sub(1), current_revision),
    ] {
        core.audio_event(AudioEvent::SeekRejected {
            revision,
            snapshot: audio::AudioSnapshot {
                generation,
                position_seconds: 0.25,
                listened_seconds: 9.0,
            },
            message: "Obsolete rejection must not be published".into(),
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        assert_eq!(core.state.playback.position, 0.5);
        assert_eq!(core.state.playback.seek_revision, current_revision);
        assert!(core.state.system.last_error.is_none());
        assert_eq!(core.transport.session.as_ref().unwrap().heard, 0.0);
    }
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Seek { revision, .. } if revision == rejected_revision
    ));
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Seek { seconds: 0.5, revision } if revision == current_revision
    ));
    core.command(Command::Seek { seconds: 1e16 }).unwrap();
    let newest_revision = core.state.playback.seek_revision;
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Seek { revision, .. } if revision == newest_revision
    ));
    for (revision, position_seconds, message) in [
        (newest_revision, 0.5, "Current backend rejection"),
        (rejected_revision, 0.25, "Obsolete backend rejection"),
    ] {
        core.audio_event(AudioEvent::SeekRejected {
            revision,
            snapshot: audio::AudioSnapshot {
                generation: core.transport.generation,
                position_seconds,
                listened_seconds: 0.0,
            },
            message: message.into(),
        })
        .unwrap();
        assert_eq!(core.state.playback.position, 0.5);
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        assert_eq!(
            core.state.playback.seek_revision,
            newest_revision.wrapping_add(1)
        );
        assert_eq!(
            core.state.system.last_error.as_deref(),
            Some("Current backend rejection")
        );
    }
    assert!(commands.try_recv().is_err());
}

#[test]
fn queued_pause_is_authoritative_for_navigation_not_the_callers_snapshot() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
    let expected = core.store.queue_id_at(2).unwrap().unwrap();
    let commands = recording_audio(&mut core);
    let (handle, control, worker, publications) = navigation_runtime(core);
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Playing
    );
    handle.send(Command::Pause).unwrap();
    assert!(futures::executor::block_on(handle.request_ack(Command::Next)).is_success());
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
fn shuffle_previous_preserves_status_and_follows_occurrence_history() {
    for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
        let (_directory, mut core) = navigation_fixture(status);
        let ids = core.store.queue_ids().unwrap();
        pending(&mut core, ids[3]);
        core.state.playback.status = status;
        if status == PlaybackStatus::Stopped {
            core.transport.session = None;
        }
        edit_preferences(&mut core, |config| config.shuffle = true);
        core.transport.history = vec![ids[0], ids[2], ids[3]];
        core.transport.history_cursor = 3;
        let commands = recording_audio(&mut core);
        let (handle, control, worker, publications) = navigation_runtime(core);
        for expected in [ids[2], ids[0]] {
            assert!(
                futures::executor::block_on(handle.request_ack(Command::Previous)).is_success()
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
        assert_eq!(core.transport.history, [ids[0], ids[2], ids[3]]);
        assert_eq!(core.transport.history_cursor, 1);
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
                core.store.queue_id_at(0).unwrap().unwrap()
            } else if repeat == RepeatMode::All {
                core.store.queue_id_at(0).unwrap().unwrap()
            } else {
                // Explicit Next ignores Repeat One; natural EOF honors it.
                core.store.queue_id_at(2).unwrap().unwrap()
            };
            let current = if shuffle {
                expected
            } else if repeat == RepeatMode::All {
                *core.store.queue_ids().unwrap().last().unwrap()
            } else {
                core.store.queue_id_at(1).unwrap().unwrap()
            };
            pending(&mut core, current);
            core.state.playback.status = status;
            if status == PlaybackStatus::Stopped {
                core.transport.session = None;
            }
            edit_preferences(&mut core, |config| config.shuffle = shuffle);
            edit_preferences(&mut core, |config| config.repeat = repeat);
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            assert!(futures::executor::block_on(handle.request_ack(Command::Next)).is_success());
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
fn previous_without_predecessor_preserves_position_and_status() {
    for status in [
        PlaybackStatus::Stopped,
        PlaybackStatus::Paused,
        PlaybackStatus::Playing,
    ] {
        for shuffle in [false, true] {
            let (_directory, mut core) = navigation_fixture(status);
            let current = core.store.queue_id_at(0).unwrap().unwrap();
            pending(&mut core, current);
            core.state.playback.status = status;
            if status == PlaybackStatus::Stopped {
                core.transport.session = None;
            }
            core.state.playback.position = 4.0;
            edit_preferences(&mut core, |config| config.shuffle = shuffle);
            let commands = recording_audio(&mut core);
            core.command(Command::Previous).unwrap();
            assert_eq!(core.state.queue.current_id, Some(current));
            assert_eq!(core.state.playback.status, status);
            assert_eq!(core.state.playback.position, 4.0);
            assert!(commands.try_recv().is_err());
        }
    }
}

#[test]
fn ended_missing_next_stops_and_resume_attempts_a_load() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.state.playback.position = 10.0;
    let generation = core.transport.generation;
    let error = core
        .audio_event(AudioEvent::Ended {
            generation,
            completed_duration: None,
            bitrate_bps: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("2.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.playback.position, 0.0);
    assert!(core.transport.session.is_none());
    assert_eq!(core.store.history(1).unwrap()[0].track_id, 1);
    assert_ne!(core.transport.generation, generation);
    core.audio_event(AudioEvent::Progress {
        generation,
        seek_revision: 0,
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
    let generation = core.transport.generation;
    let played_at = core.store.history(1).unwrap()[0].played_at;
    missing(&mut core, Command::Next, 2);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(core.state.playback.position, 4.0);
    assert_eq!(core.transport.generation, generation);
    assert_eq!(core.transport.session.as_ref().unwrap().track_id, 1);
    assert_eq!(core.store.history(1).unwrap()[0].played_at, played_at);
}

#[test]
fn repeat_one_only_repeats_natural_end_and_repeat_all_revisits_singleton() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    edit_preferences(&mut core, |config| config.repeat = RepeatMode::One);
    missing(&mut core, Command::Next, 2);
    let generation = core.transport.generation;
    let error = core
        .audio_event(AudioEvent::Ended {
            generation,
            completed_duration: None,
            bitrate_bps: None,
        })
        .unwrap_err();
    assert!(error.to_string().contains("1.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    core.command(Command::RemoveTracks {
        track_ids: vec![2, 3, 4],
    })
    .unwrap();
    edit_preferences(&mut core, |config| config.shuffle = true);
    edit_preferences(&mut core, |config| config.repeat = RepeatMode::All);
    // EOF stopped playback above; model an active singleton before explicit Next.
    playing(&mut core, 1);
    core.transport.history = vec![1];
    core.transport.history_cursor = 1;
    missing(&mut core, Command::Next, 1);
}

#[test]
fn random_draws_include_current_weight_duplicates_and_ignore_forward_history() {
    use rand::SeedableRng;
    let (_directory, mut core) = fixture();
    core.state.queue.current_id = Some(3);
    core.transport.history = vec![1, 3, 5];
    core.transport.history_cursor = 1;
    let mut rng = rand::rngs::StdRng::seed_from_u64(9);
    let mut counts = [0usize; 5];
    let mut immediate_repeat = false;
    let mut previous = 3;
    for _ in 0..30_000 {
        let id = core.random_queue_id(&mut rng).unwrap().unwrap();
        immediate_repeat |= previous == id;
        counts[id as usize - 1] += 1;
        previous = id;
    }
    assert!(immediate_repeat);
    assert!(counts.iter().all(|count| (5_500..6_500).contains(count)));
    assert!(counts[1] + counts[2] > counts[0] * 18 / 10);
    core.command(Command::ClearQueue).unwrap();
    core.enqueue(&[1]).unwrap();
    let id = core.store.queue_id_at(0).unwrap().unwrap();
    assert_eq!(core.random_queue_id(&mut rng).unwrap(), Some(id));
    for _ in 0..5000 {
        core.transport.record_history(id);
    }
    assert_eq!(core.transport.history.len(), 4096);
    assert_eq!(core.transport.history_cursor, 4096);
}

#[test]
fn repeated_keyboard_intents_accumulate_from_one_snapshot_in_execution_order() {
    use rivu_presentation::keyboard::{self, Action};

    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    edit_preferences(&mut core, |config| config.volume = 0.4);
    edit_preferences(&mut core, |config| config.repeat = RepeatMode::Off);
    edit_preferences(&mut core, |config| config.shuffle = false);
    core.state.playback.position = 2.0;
    let snapshot = core.state.playback.clone();
    let occurrence = core.state.queue.current_id;
    let input = |action| keyboard::playback_command(action, &snapshot, occurrence).unwrap();

    for _ in 0..2 {
        core.command(input(Action::VolumeUp)).unwrap();
        core.command(input(Action::SeekForward)).unwrap();
    }
    assert!((core.preferences.live().volume - 0.5).abs() < 0.000001);
    assert_eq!(core.state.playback.position, 12.0);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    for _ in 0..2 {
        core.command(input(Action::VolumeDown)).unwrap();
        core.command(input(Action::SeekBackward)).unwrap();
    }
    assert!((core.preferences.live().volume - 0.4).abs() < 0.000001);
    assert_eq!(core.state.playback.position, 2.0);
    for expected in [
        RepeatMode::All,
        RepeatMode::One,
        RepeatMode::Off,
        RepeatMode::All,
    ] {
        core.command(input(Action::Repeat)).unwrap();
        assert_eq!(core.preferences.live().repeat, expected);
    }
    for expected in [true, false, true] {
        core.command(input(Action::Shuffle)).unwrap();
        assert_eq!(core.preferences.live().shuffle, expected);
    }
    core.command(Command::Repeat {
        mode: RepeatMode::One,
    })
    .unwrap();
    core.command(input(Action::Repeat)).unwrap();
    assert_eq!(core.preferences.live().repeat, RepeatMode::Off);
    core.command(Command::Shuffle { enabled: false }).unwrap();
    core.command(input(Action::Shuffle)).unwrap();
    assert!(core.preferences.live().shuffle);
}

#[test]
fn relative_intents_clamp_owner_values_and_reject_nonfinite_deltas() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    let queue_id = core.state.queue.current_id.unwrap();
    for (volume, delta, expected) in [(0.98, 0.05, 1.0), (0.02, -0.05, 0.0)] {
        core.command(Command::Volume { value: volume }).unwrap();
        core.command(Command::AdjustVolume { delta }).unwrap();
        assert_eq!(core.preferences.live().volume, expected);
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
    let queue_id = start_cue_navigation(&mut core);
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
        assert_eq!(core.preferences.live().volume, 0.0);
    }
}

#[test]
fn delayed_keyboard_seeks_cannot_target_next_duplicate_occurrence_or_stopped_playback() {
    use rivu_presentation::keyboard::{self, Action};

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

#[test]
fn mpris_relative_seeks_accumulate_at_execution_and_keep_pause() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let _commands = recording_audio(&mut core);
    let (handle, control, worker, _) = navigation_runtime(core);
    let queue_id = handle.mpris_snapshot().queue.current_id.unwrap();
    handle.send(Command::Seek { seconds: 2.0 }).unwrap();
    for expected in [3.25, 4.5] {
        let ack = futures::executor::block_on(handle.mpris_seek(queue_id, 1_250_000));
        assert!(ack.is_success(), "{:?}", ack.error);
        let state = handle.mpris_snapshot();
        assert_eq!(state.playback.position, expected);
        assert_eq!(state.playback.status, PlaybackStatus::Paused);
        assert_eq!(state.queue.current_id, Some(queue_id));
    }
    assert!(futures::executor::block_on(handle.mpris_seek(queue_id, -10_000_000)).is_success());
    assert_eq!(handle.mpris_snapshot().playback.position, 0.0);
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn mpris_over_end_seek_is_occurrence_bound_next_with_tail_noop() {
    for status in [PlaybackStatus::Playing, PlaybackStatus::Paused] {
        let (_directory, mut core) = navigation_fixture(status);
        let _commands = recording_audio(&mut core);
        let first = core.state.queue.current_id.unwrap();
        let duplicate = core.store.queue_id_at(2).unwrap().unwrap();
        let following: Vec<_> = (3..core.state.queue.total)
            .map(|index| core.store.queue_id_at(index).unwrap().unwrap())
            .collect();
        let (handle, control, worker, _) = navigation_runtime(core);
        assert!(futures::executor::block_on(handle.mpris_seek(first, 10_000_000)).is_success());
        assert_eq!(handle.mpris_snapshot().queue.current_id, Some(first));
        assert_eq!(handle.mpris_snapshot().playback.position, 10.0);
        assert!(futures::executor::block_on(handle.mpris_seek(first, 1)).is_success());
        let state = handle.mpris_snapshot();
        assert_eq!(state.queue.current_id, Some(duplicate));
        assert_eq!(state.playback.status, status);
        assert_eq!(state.playback.position, 0.0);
        let revision = state.playback.seek_revision;
        // This occurrence contains the same track, but an older relative
        // intent must neither seek it nor navigate past it.
        for offset in [1_000_000, i64::MAX] {
            assert!(futures::executor::block_on(handle.mpris_seek(first, offset)).is_success());
            let state = handle.mpris_snapshot();
            assert_eq!(state.queue.current_id, Some(duplicate));
            assert_eq!(state.playback.position, 0.0);
            assert_eq!(state.playback.seek_revision, revision);
        }
        let mut tail = duplicate;
        for next in following {
            assert!(futures::executor::block_on(handle.mpris_seek(tail, i64::MAX)).is_success());
            let state = handle.mpris_snapshot();
            assert_eq!(state.queue.current_id, Some(next));
            assert_eq!(state.playback.status, status);
            assert_eq!(state.playback.position, 0.0);
            tail = next;
        }
        assert!(futures::executor::block_on(handle.mpris_seek(tail, i64::MAX)).is_success());
        let state = handle.mpris_snapshot();
        assert_eq!(state.queue.current_id, Some(tail));
        assert_eq!(state.playback.status, status);
        assert_eq!(state.playback.position, 0.0);
        assert!(futures::executor::block_on(handle.request_ack(Command::Stop)).is_success());
        let revision = handle.mpris_snapshot().playback.seek_revision;
        assert!(futures::executor::block_on(handle.mpris_seek(tail, i64::MAX)).is_success());
        let state = handle.mpris_snapshot();
        assert_eq!(state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(state.playback.seek_revision, revision);
        control.request_shutdown().unwrap();
        worker.join().unwrap();
    }
}

#[test]
fn mpris_over_end_seek_recovers_an_appended_old_tail_without_replaying_enqueue() {
    for status in [PlaybackStatus::Playing, PlaybackStatus::Paused] {
        let (directory, mut core) = navigation_fixture(status);
        pending(&mut core, 5);
        core.state.playback.status = status;
        let connection = fail_queue_projection_after(directory.path(), "INSERT");
        let error = core
            .command(Command::Enqueue { track_ids: vec![1] })
            .unwrap_err();
        assert!(error.to_string().contains("do not repeat"));
        assert_eq!(core.state.queue.current_position, Some(4));
        assert_eq!(core.state.queue.total, 5);
        assert!(core.transport.projection_stale);
        let saved = core.store.queue_ids().unwrap();
        assert_eq!(saved.len(), 6);
        let revision = core.state.queue.revision;
        let commands = recording_audio(&mut core);
        let seek = |core: &mut Core, offset| {
            operation_ack(
                core,
                Operation::MprisSeek {
                    queue_id: 5,
                    offset,
                },
            )
        };
        // A broken queue projection cannot affect a local seek, but the old
        // scalar tail must not suppress an over-end recovery failure.
        assert!(seek(&mut core, 1_000_000).is_success());
        assert_eq!(core.state.playback.position, 1.0);
        assert!(!seek(&mut core, i64::MAX).is_success());
        assert_eq!(core.state.queue.current_id, Some(5));
        assert_eq!(core.state.playback.status, status);
        assert!(core.transport.projection_stale);
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Seek { seconds: 1.0, .. }
        ));
        assert!(commands.try_recv().is_err());
        repair_queue_projection(&connection);
        assert!(seek(&mut core, i64::MAX).is_success());
        assert!(!core.transport.projection_stale);
        assert_eq!(core.shared.read().queue.current_id, saved.last().copied());
        assert_eq!(core.state.queue.current_position, Some(5));
        assert_eq!(core.state.queue.total, 6);
        assert_eq!(core.state.playback.position, 0.0);
        assert_eq!(core.state.playback.status, status);
        assert_eq!(core.state.queue.revision, revision);
        assert_eq!(core.store.queue_ids().unwrap(), saved);
        assert_navigation_audio(&commands, status, &directory.path().join("1.wav"));
        assert_queue_saved(&core);
    }
}

#[test]
fn mpris_over_end_seek_recovers_a_surviving_tail_after_committed_removal() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    pending(&mut core, 5);
    core.state.playback.status = PlaybackStatus::Paused;
    let connection = committed_queue_scalar_refresh_failure(&mut core, directory.path());
    repair_queue_scalar(&connection);
    let generation = core.transport.generation;
    let commands = recording_audio(&mut core);
    let ack = operation_ack(
        &mut core,
        Operation::MprisSeek {
            queue_id: 5,
            offset: i64::MAX,
        },
    );
    assert!(ack.is_success(), "{:?}", ack.error);
    assert!(!core.transport.projection_stale);
    assert_eq!(core.shared.read().queue.total, 3);
    assert_eq!(core.state.queue.current_id, Some(5));
    assert_eq!(core.state.queue.current_position, Some(2));
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert_eq!(core.transport.generation, generation);
    assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
    assert!(commands.try_recv().is_err());
}

#[test]
fn stale_queue_toggle_recovers_before_stopped_selection() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    // Select a surviving occurrence; Toggle's Resume path must recover the
    // committed queue projection before selecting or opening a track.
    core.state.queue.current_id = Some(1);
    core.save_queue().unwrap();
    core.refresh_queue_tracks().unwrap();
    let connection = committed_queue_scalar_refresh_failure(&mut core, directory.path());
    let _commands = recording_audio(&mut core);
    assert!(core.command(Command::Toggle).is_err());
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    repair_queue_scalar(&connection);
    core.command(Command::Toggle).unwrap();
    assert!(!core.transport.projection_stale);
    assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
    assert_eq!(core.state.queue.total, 3);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
}

#[test]
fn stale_queue_navigation_operation_recovers_order_before_selecting() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    core.state.queue.current_id = Some(1);
    core.save_queue().unwrap();
    core.refresh_queue_tracks().unwrap();
    let connection = committed_queue_scalar_refresh_failure(&mut core, directory.path());
    repair_queue_scalar(&connection);
    let _commands = recording_audio(&mut core);
    let (handle, control, worker, _publications) = navigation_runtime(core);
    let ack = futures::executor::block_on(handle.request_ack(Command::Next));
    assert!(ack.is_success(), "{:?}", ack.error);
    let state = handle.mpris_snapshot();
    assert_eq!(state.queue.current_id, Some(4));
    assert_eq!(state.queue.total, 3);
    assert_eq!(state.playback.status, PlaybackStatus::Stopped);
    control.request_shutdown().unwrap();
    let core = worker.join().unwrap();
    assert!(!core.transport.projection_stale);
    assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
}

#[test]
fn stale_queue_natural_eof_recovers_order_and_keeps_paused_intent() {
    for status in [PlaybackStatus::Playing, PlaybackStatus::Paused] {
        let (directory, mut core) = navigation_fixture(status);
        pending(&mut core, 1);
        core.state.playback.status = status;
        let connection = committed_queue_scalar_refresh_failure(&mut core, directory.path());
        repair_queue_scalar(&connection);
        let _commands = recording_audio(&mut core);
        core.audio_event(AudioEvent::Ended {
            generation: core.transport.generation,
            completed_duration: None,
            bitrate_bps: None,
        })
        .unwrap();
        assert!(!core.transport.projection_stale);
        assert_eq!(core.state.queue.current_id, Some(4));
        assert_eq!(core.state.queue.total, 3);
        assert_eq!(core.state.playback.status, status);
    }
}

#[test]
fn stale_queue_shuffle_previous_prunes_deleted_occurrences_before_reading_cursor() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    core.state.queue.current_id = Some(4);
    core.save_queue().unwrap();
    core.refresh_queue_tracks().unwrap();
    edit_preferences(&mut core, |config| config.shuffle = true);
    core.transport.history = vec![1, 2, 3, 4, 5];
    core.transport.history_cursor = 4;
    let connection = committed_queue_scalar_refresh_failure(&mut core, directory.path());
    repair_queue_scalar(&connection);
    core.command(Command::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(core.transport.history, [1, 4, 5]);
    assert_eq!(core.transport.history_cursor, 1);
}

#[test]
fn stopped_navigation_write_failure_preserves_new_occurrence_until_dedup_can_commit() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    let commands = recording_audio(&mut core);
    let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(core.command(Command::Next).is_err());
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(2));
    assert_eq!(core.state.current_track().unwrap().id, 2);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.store.queue_state().unwrap().current_id, Some(2));
    assert!(core.command(Command::DeduplicateQueue).is_err());
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.store.queue_ids().unwrap(), [1, 2, 3, 4, 5]);
    // A pending database write cannot prevent releasing the audio owner.
    core.command(Command::Stop).unwrap();
    assert_eq!(core.state.queue.current_id, Some(3));
    lock.execute_batch("ROLLBACK").unwrap();
    core.command(Command::DeduplicateQueue).unwrap();
    assert_eq!(queue_entries(&core), [(1, 1), (3, 2), (4, 3), (5, 4)]);
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(1));
    assert_queue_saved(&core);
    assert!(
        commands
            .try_iter()
            .all(|command| matches!(command, AudioCommand::StopAndSnapshot(_)))
    );
}

#[test]
fn authoritative_queue_removal_retires_unsaved_selection_without_resurrecting_it() {
    for clear in [false, true] {
        let (directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
        let _commands = recording_audio(&mut core);
        let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(core.command(Command::Next).is_err());
        assert_eq!(core.state.queue.current_id, Some(3));
        lock.execute_batch("ROLLBACK").unwrap();
        core.command(if clear {
            Command::ClearQueue
        } else {
            Command::RemoveTracks { track_ids: vec![2] }
        })
        .unwrap();
        core.ensure_queue_control_ready().unwrap();
        assert_eq!(core.state.queue.current_id, None);
        assert!(core.state.current_track().is_none());
        assert_eq!(
            core.store.queue_ids().unwrap(),
            if clear { vec![] } else { vec![1, 4, 5] }
        );
        assert_queue_saved(&core);
    }
}
#[test]
fn authoritative_queue_replacement_supersedes_unsaved_old_selection() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    let playlist_id = core
        .store
        .create_playlist_with_tracks("Replacement", &[1, 4])
        .unwrap();
    let _commands = recording_audio(&mut core);
    let lock = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(core.command(Command::Next).is_err());
    assert_eq!(core.state.queue.current_id, Some(3));
    lock.execute_batch("ROLLBACK").unwrap();
    core.command(Command::PlayPlaylist { playlist_id }).unwrap();
    assert_eq!(
        queue_entries(&core)
            .iter()
            .map(|&(_, id)| id)
            .collect::<Vec<_>>(),
        [1, 4]
    );
    assert_eq!(
        core.state.queue.current_id,
        core.store.queue_id_at(0).unwrap()
    );
    assert_ne!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.current_track().unwrap().id, 1);
    assert_queue_saved(&core);
}

#[test]
fn accepted_load_survives_selection_save_failure_and_late_projection() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
    let old_source = core.transport.generation;
    let old_selection = core.store.queue_state().unwrap().current_id;
    let delayed = core.store.queue_projection(core.state.queue.revision);
    let commands = recording_audio(&mut core);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER reject_queue_selection BEFORE UPDATE OF current_queue_entry_id ON rivu_meta
         BEGIN SELECT RAISE(ABORT,'selection rejected'); END;",
    ).unwrap();
    assert!(core.start(3, QueueHistory::Record).is_err());
    let source = core.transport.generation;
    assert_ne!(source, old_source);
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert!(core.transport.selection_pending);
    assert_eq!(core.transport.session.as_ref().unwrap().track_id, 2);
    assert_eq!(core.store.queue_state().unwrap().current_id, old_selection);
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::StopAndSnapshot(_)
    ));
    assert!(
        matches!(commands.recv_timeout(Duration::from_secs(2)).unwrap(), AudioCommand::Load { generation, .. } if generation == source)
    );
    assert!(core.accept_queue_projection(delayed).is_err());
    assert_eq!(core.state.queue.current_id, Some(3));
    connection
        .execute_batch("DROP TRIGGER reject_queue_selection")
        .unwrap();
    core.ensure_queue_control_ready().unwrap();
    assert_queue_saved(&core);
    assert_eq!(core.transport.generation, source);
    assert_eq!(core.state.queue.current_id, Some(3));
    assert!(
        commands.try_recv().is_err(),
        "selection recovery must not replay Load"
    );
    core.audio_event(AudioEvent::Ended {
        generation: old_source,
        completed_duration: None,
        bitrate_bps: None,
    })
    .unwrap();
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
}

#[test]
fn shuffle_previous_commits_history_cursor_before_a_selection_save_can_fail() {
    let (directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    edit_preferences(&mut core, |config| config.shuffle = true);
    core.transport.history = vec![5, 4, 2];
    core.transport.history_cursor = 3;
    let commands = recording_audio(&mut core);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER reject_queue_selection BEFORE UPDATE OF current_queue_entry_id ON rivu_meta
         BEGIN SELECT RAISE(ABORT,'selection rejected'); END;",
    ).unwrap();
    assert!(core.command(Command::Previous).is_err());
    assert_eq!(core.state.queue.current_id, Some(4));
    assert_eq!(core.transport.history_cursor, 2);
    connection
        .execute_batch("DROP TRIGGER reject_queue_selection")
        .unwrap();
    core.ensure_queue_control_ready().unwrap();
    core.command(Command::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(5));
    assert_eq!(core.transport.history_cursor, 1);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    drop(commands);
}

#[test]
fn manual_tail_next_preserves_source_while_natural_eof_stops() {
    for status in [
        PlaybackStatus::Playing,
        PlaybackStatus::Paused,
        PlaybackStatus::Stopped,
    ] {
        let (_directory, mut core) = navigation_fixture(status);
        let tail = core
            .store
            .queue_id_at(core.state.queue.total - 1)
            .unwrap()
            .unwrap();
        pending(&mut core, tail);
        core.state.playback.status = status;
        if status == PlaybackStatus::Stopped {
            core.transport.session = None;
        }
        core.state.playback.position = 4.0;
        let generation = core.transport.generation;
        let commands = recording_audio(&mut core);
        core.command(Command::Next).unwrap();
        assert_eq!(core.state.queue.current_id, Some(tail));
        assert_eq!(core.state.playback.status, status);
        assert_eq!(core.state.playback.position, 4.0);
        assert_eq!(core.transport.generation, generation);
        assert!(commands.try_recv().is_err());
        if status != PlaybackStatus::Stopped {
            core.audio_event(AudioEvent::Ended {
                generation,
                completed_duration: Some(10.0),
                bitrate_bps: None,
            })
            .unwrap();
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            assert_eq!(core.state.playback.position, 0.0);
            assert_eq!(core.state.queue.current_id, Some(tail));
        }
    }
}

#[test]
fn empty_navigation_and_resume_do_not_send_audio_commands() {
    let (_directory, mut core) = fixture();
    core.command(Command::ClearQueue).unwrap();
    let generation = core.transport.generation;
    let commands = recording_audio(&mut core);
    for command in [Command::Next, Command::Previous, Command::Resume] {
        let ack = operation_ack(&mut core, Operation::Command(command));
        assert!(ack.is_success(), "{:?}", ack.error);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.queue.current_id, None);
        assert_eq!(core.transport.generation, generation);
        assert!(core.state.system.last_error.is_none());
        assert!(commands.try_recv().is_err());
    }
}

#[test]
fn shuffle_history_exhaustion_never_falls_back_to_physical_predecessor() {
    for status in [
        PlaybackStatus::Playing,
        PlaybackStatus::Paused,
        PlaybackStatus::Stopped,
    ] {
        for recorded in [false, true] {
            let (_directory, mut core) = navigation_fixture(status);
            let current = core.state.queue.current_id.unwrap();
            assert!(core.state.queue.current_position.unwrap() > 0);
            edit_preferences(&mut core, |config| config.shuffle = true);
            if recorded {
                core.transport.record_history(current);
            }
            core.state.playback.position = 4.0;
            let generation = core.transport.generation;
            let commands = recording_audio(&mut core);
            core.command(Command::Previous).unwrap();
            assert_eq!(core.state.queue.current_id, Some(current));
            assert_eq!(core.state.playback.status, status);
            assert_eq!(core.state.playback.position, 4.0);
            assert_eq!(core.transport.generation, generation);
            assert!(commands.try_recv().is_err());
        }
    }
}

#[test]
fn stopped_resume_preserves_history_cursor_and_forward_visits_but_explicit_play_records() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
    let ids = core.store.queue_ids().unwrap();
    edit_preferences(&mut core, |config| config.shuffle = true);
    core.transport.history = vec![ids[4], ids[1], ids[3]];
    core.transport.history_cursor = 2;
    let commands = recording_audio(&mut core);
    for _ in 0..2 {
        core.command(Command::Resume).unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        assert_eq!(core.transport.history, [ids[4], ids[1], ids[3]]);
        assert_eq!(core.transport.history_cursor, 2);
        core.command(Command::Stop).unwrap();
    }
    core.command(Command::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(ids[4]));
    core.command(Command::PlayQueue { queue_id: ids[4] })
        .unwrap();
    assert_eq!(core.transport.history, [ids[4], ids[4]]);
    assert_eq!(core.transport.history_cursor, 2);
    drop(commands);
}

#[test]
fn queued_resume_uses_execution_status_and_playing_resume_is_a_noop() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
    let commands = recording_audio(&mut core);
    let (handle, control, worker, _) = navigation_runtime(core);
    handle.send(Command::Pause).unwrap();
    assert!(futures::executor::block_on(handle.request_ack(Command::Resume)).is_success());
    assert_eq!(
        handle.mpris_snapshot().playback.status,
        PlaybackStatus::Playing
    );
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Pause(true)
    ));
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Pause(false)
    ));
    assert!(futures::executor::block_on(handle.request_ack(Command::Resume)).is_success());
    assert!(commands.try_recv().is_err());
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn mpris_set_position_uses_execution_duration_and_ignores_stale_or_stopped_occurrences() {
    let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
    let queue_id = core.state.queue.current_id.unwrap();
    let commands = recording_audio(&mut core);
    let intent = Operation::MprisSetPosition {
        queue_id,
        position: 5_000_001,
    };
    // Decoder duration replaces the 10-second library estimate before execution.
    started(&mut core, Some(5.0)).unwrap();
    let revision = core.state.playback.seek_revision;
    assert!(operation_ack(&mut core, intent).is_success());
    assert_eq!(core.state.playback.seek_revision, revision);
    assert!(commands.try_recv().is_err());
    for position in [-1, 5_000_001, i64::MAX] {
        let ack = operation_ack(
            &mut core,
            Operation::MprisSetPosition { queue_id, position },
        );
        assert!(ack.is_success(), "{:?}", ack.error);
        assert_eq!(core.state.playback.seek_revision, revision);
        assert!(commands.try_recv().is_err());
    }
    for position in [1_250_000, 5_000_000] {
        let ack = operation_ack(
            &mut core,
            Operation::MprisSetPosition { queue_id, position },
        );
        assert!(ack.is_success(), "{:?}", ack.error);
        let seconds = position as f64 / 1_000_000.0;
        assert_eq!(core.state.playback.position, seconds);
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Seek { seconds: actual, .. } if actual == seconds
        ));
    }
    // Ordinary CLI seeks retain decoder authority instead of estimate clamping.
    core.command(Command::Seek { seconds: 25.0 }).unwrap();
    assert_eq!(core.state.playback.position, 25.0);
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Seek { seconds: 25.0, .. }
    ));
    core.command(Command::Next).unwrap();
    assert_ne!(core.state.queue.current_id, Some(queue_id));
    let current = core.state.queue.current_id.unwrap();
    assert_eq!(core.state.current_track().unwrap().id, 2);
    let revision = core.state.playback.seek_revision;
    assert!(
        operation_ack(
            &mut core,
            Operation::MprisSetPosition {
                queue_id,
                position: 1_000_000,
            },
        )
        .is_success()
    );
    assert_eq!(core.state.playback.seek_revision, revision);
    core.command(Command::Stop).unwrap();
    let revision = core.state.playback.seek_revision;
    assert!(
        operation_ack(
            &mut core,
            Operation::MprisSetPosition {
                queue_id: current,
                position: 1_000_000,
            },
        )
        .is_success()
    );
    assert_eq!(core.state.playback.seek_revision, revision);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
}

#[test]
fn mpris_set_position_allows_unknown_duration_without_queue_recovery() {
    let (_directory, mut core) = fixture_with_duration(None);
    pending(&mut core, 1);
    core.state.playback.status = PlaybackStatus::Paused;
    core.transport.selection_pending = true;
    core.transport.projection_stale = true;
    let commands = recording_audio(&mut core);
    assert!(
        operation_ack(
            &mut core,
            Operation::MprisSetPosition {
                queue_id: 1,
                position: 1_250_000,
            },
        )
        .is_success()
    );
    assert_eq!(core.state.playback.position, 1.25);
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    assert!(core.transport.selection_pending);
    assert!(core.transport.projection_stale);
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(2)).unwrap(),
        AudioCommand::Seek { seconds: 1.25, .. }
    ));
    assert!(commands.try_recv().is_err());
}
