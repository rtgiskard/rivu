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
            let ack = handle.navigate(direction);
            assert!(ack.ok, "{:?}", ack.error);
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
                    .queue_projection(core.queue_ids.len())
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
        let (snapshot, recorded_through_snapshot) = bounded(1);
        core.audio(AudioCommand::StopAndSnapshot(snapshot)).unwrap();
        recorded_through_snapshot
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
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
    assert!(handle.navigate(Navigation::Next).ok);
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
        assert!(handle.navigate(Navigation::Previous).ok);
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
            assert!(handle.navigate(Navigation::Previous).ok);
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
            assert!(handle.request_ack(command).ok);
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
                core.store.clear_queue().unwrap();
                core.reload_queue().unwrap();
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
            assert!(handle.navigate(Navigation::Next).ok);
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
    assert!(handle.navigate(Navigation::Previous).ok);
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
