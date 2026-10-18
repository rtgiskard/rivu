use super::*;

#[test]
fn committed_play_count_invalidates_and_is_not_replayed_after_projection_failure() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    playing(&mut core, 1);
    let before_count = core.state.current_track().unwrap().play_count;
    let before = core.state.query_revisions();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_stats_refresh AFTER UPDATE OF revision ON queue_meta
         WHEN typeof(NEW.revision)='integer'
         BEGIN UPDATE queue_meta SET revision='unreadable' WHERE singleton=1; END;",
        )
        .unwrap();
    let error = core.playback_progress(8.0, 8.0).unwrap_err();
    assert!(error.to_string().contains("Playback statistics committed"));
    assert_eq!(core.state.library.revision, before.library + 1);
    assert_eq!(
        core.state.current_track().unwrap().play_count,
        before_count + 1
    );
    assert!(core.playback.as_ref().unwrap().counted);
    let count = || {
        connection
            .query_row("SELECT play_count FROM tracks WHERE id=1", [], |row| {
                row.get::<_, i64>(0)
                    .map(|value| u64::try_from(value).unwrap())
            })
            .unwrap()
    };
    assert_eq!(count(), before_count + 1);
    core.playback_progress(9.0, 9.0).unwrap();
    assert_eq!(count(), before_count + 1);
    assert_eq!(core.state.library.revision, before.library + 1);
    // Queue tokens are persisted authority, never synthesized from a failed read.
    assert_eq!(core.state.queue.revision, before.queue);
    connection
        .execute_batch("DROP TRIGGER fail_stats_refresh;")
        .unwrap();
    connection
        .execute(
            "UPDATE queue_meta SET revision=?1 WHERE singleton=1",
            [i64::try_from(before.queue + 1).unwrap()],
        )
        .unwrap();
    core.refresh_queue_tracks().unwrap();
    assert_eq!(core.state.queue.revision, before.queue + 1);
    assert_eq!(
        core.state.current_track().unwrap().play_count,
        before_count + 1
    );
}

#[test]
fn saturated_audio_admission_preserves_state_requests_events_and_command_order() {
    let (_directory, mut core) = fixture();
    let (commands, receive) = bounded(64);
    core.engine.replace_commands(commands.clone());
    core.command(Command::Device {
        name: Some("accepted-output".into()),
    })
    .unwrap();
    core.flush_config().unwrap();
    for index in 1..64 {
        commands.send(AudioCommand::Volume(index as f32)).unwrap();
    }
    let (events, event_receive) = bounded(128);
    for index in 0..128 {
        events
            .send(AudioEvent::OutputSettingsFailed {
                request: core.output_settings_request,
                device: core.state.system.selected_device.clone(),
                auto_mix: core.state.system.config.pipewire_auto_mix,
                message: format!("accepted failure {index}"),
            })
            .unwrap();
    }
    core.engine.events = event_receive;
    let saved = std::fs::read(&core.state.system.config_path).unwrap();
    let generation = core.generation;
    let (finished, completion) = bounded(1);
    let worker = thread::spawn(move || {
        let result = core.command(Command::Device {
            name: Some("rejected-output".into()),
        });
        finished
            .send((core, result))
            .unwrap_or_else(|_| panic!("Core result receiver stopped"));
    });
    let (mut core, result) = match completion.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => result,
        Err(error) => {
            // Unblock the old blocking-send implementation before failing, so
            // this deterministic regression never leaves a parked Core thread.
            drop(receive);
            worker.join().unwrap();
            panic!("saturated Core command did not return: {error}");
        }
    };
    worker.join().unwrap();
    assert!(result.unwrap_err().to_string().contains("not queued"));
    assert_eq!(core.output_settings_request, 1);
    assert_eq!(core.generation, generation);
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("accepted-output")
    );
    assert_eq!(
        std::fs::read(&core.state.system.config_path).unwrap(),
        saved
    );

    core.state.playback.position = 2.0;
    let volume = core.state.playback.volume;
    let seek_revision = core.state.playback.seek_revision;
    for (command, status) in [
        (Command::Volume { value: 0.2 }, PlaybackStatus::Playing),
        (Command::Pause, PlaybackStatus::Playing),
        (Command::Resume, PlaybackStatus::Paused),
        (Command::Seek { seconds: 5.0 }, PlaybackStatus::Playing),
    ] {
        core.state.playback.status = status;
        assert!(
            core.command(command)
                .unwrap_err()
                .to_string()
                .contains("not queued")
        );
        assert_eq!(core.state.playback.status, status);
        assert_eq!(core.state.playback.position, 2.0);
        assert_eq!(core.state.playback.seek_revision, seek_revision);
        assert_eq!(core.state.playback.volume, volume);
    }
    assert_eq!(receive.len(), 64);
    assert_eq!(core.engine.events.len(), 128);
    assert!(matches!(
        receive.try_recv().unwrap(),
        AudioCommand::OutputSettings { request: 1, device: Some(device), .. }
            if device == "accepted-output"
    ));
    for index in 1..64 {
        assert!(matches!(
            receive.try_recv().unwrap(),
            AudioCommand::Volume(value) if value == index as f32
        ));
    }
    assert!(receive.try_recv().is_err());
    for index in 0..128 {
        let event = core.engine.events.try_recv().unwrap();
        core.audio_event(event).unwrap();
        assert_eq!(
            core.state.system.last_error,
            Some(format!("accepted failure {index}"))
        );
    }
    core.command(Command::Device {
        name: Some("next-accepted-output".into()),
    })
    .unwrap();
    assert!(matches!(
        receive.try_recv().unwrap(),
        AudioCommand::OutputSettings { request: 2, .. }
    ));
    assert!(
        receive.try_recv().is_err(),
        "rejected commands must not be replayed"
    );
    drop(receive);
    assert!(core.command(Command::Volume { value: 0.3 }).is_err());
    assert_eq!(core.state.playback.volume, volume);
}

#[test]
fn activity_updates_playback_but_only_display_statistics_invalidate_queue_pages() {
    let (_directory, mut core) = fixture();
    core.store.set_queue_current(Some(1)).unwrap();
    core.refresh_queue_tracks().unwrap();
    let revision = core.state.queue.revision;
    let stats = core.store.mark_played(1, 7).unwrap();
    core.update_track_stats(1, stats).unwrap();
    assert_eq!(core.state.current_track().unwrap().last_played, Some(7));
    assert_eq!(core.state.queue.revision, revision);
    let stats = core.store.increment_play_count(1).unwrap();
    core.update_track_stats(1, stats).unwrap();
    assert_ne!(core.state.queue.revision, revision);
    assert_eq!(
        core.store.queue_page(None, 0, 1).unwrap().rows[0]
            .track
            .play_count,
        1
    );
}

#[test]
fn playback_publication_does_not_invalidate_unchanged_sections() {
    let (_directory, mut core) = fixture();
    core.publish();
    let before = core.state.revisions;
    let wakeup_revision = core.shared.read().system.revision;
    core.state.playback.position = 2.0;
    core.publish();
    assert_eq!(
        core.state.revisions.changed_since(before),
        StateSections {
            playback: true,
            ..StateSections::default()
        }
    );
    {
        let published = core.shared.read();
        assert_eq!(published.revisions.system, before.system);
        assert_ne!(published.system.revision, wakeup_revision);
    }
    let before = core.state.revisions;
    core.state.system.last_error = Some("failed scan".into());
    core.publish();
    assert_eq!(
        core.state.revisions.changed_since(before),
        StateSections {
            system: true,
            ..StateSections::default()
        }
    );
}

fn precision_pending(core: &mut Core) {
    let track = core.store.track(1).unwrap().unwrap();
    std::fs::write(&track.path, b"encoded source").unwrap();
    let metadata = std::fs::metadata(&track.path).unwrap();
    let mut media = core
        .store
        .known_source(&track.path)
        .unwrap()
        .unwrap()
        .media
        .unwrap();
    media.bitrate_bps = Some(128_000);
    media.bitrate_exact = false;
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records: vec![ScanRecord {
                path: track.path,
                cue: None,
                size: metadata.len(),
                modified_ns: metadata
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64,
                fingerprint: None,
                media,
            }],
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    core.store.clear_queue().unwrap();
    core.reload_queue().unwrap();
    core.enqueue(&[1]).unwrap();
    let queue_id = core.queue_ids[0];
    pending(core, queue_id);
    assert!(core.playback.as_ref().unwrap().source.is_some());
}

#[test]
fn playback_started_upgrades_estimate_and_duplicate_cannot_downgrade() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut event {
        info.bitrate_bps = Some(128_000);
        info.bitrate_exact = true;
    }
    let before = core.state.library.revision;
    core.audio_event(event).unwrap();
    assert!(core.store.track(1).unwrap().unwrap().bitrate_exact);
    assert!(core.state.current_track().unwrap().bitrate_exact);
    assert!(
        core.store.queue_page(None, 0, 10).unwrap().rows[0]
            .track
            .bitrate_exact
    );
    // The precision update shares Started's existing content revision.
    assert_eq!(core.state.library.revision, before.wrapping_add(1));
    let revision = core.state.library.revision;
    let mut duplicate = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut duplicate {
        info.bitrate_bps = Some(64_000);
    }
    core.audio_event(duplicate).unwrap();
    assert_eq!(core.state.library.revision, revision);
    assert_eq!(
        core.store.track(1).unwrap().unwrap().bitrate_bps,
        Some(128_000)
    );
}

#[test]
fn stale_playback_start_cannot_upgrade_precision() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    let revision = core.state.library.revision;
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { generation, info } = &mut event {
        *generation = generation.wrapping_sub(1);
        info.bitrate_bps = Some(192_000);
        info.bitrate_exact = true;
    }
    core.audio_event(event).unwrap();
    assert_eq!(core.state.library.revision, revision);
    assert!(!core.playback.as_ref().unwrap().started);
    assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
    assert_eq!(
        core.store.track(1).unwrap().unwrap().bitrate_bps,
        Some(128_000)
    );
}

#[test]
fn full_eof_upgrades_estimate_but_stale_and_stopped_events_do_not() {
    for drain in [false, true] {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        started(&mut core, Some(10.0)).unwrap();
        let generation = core.generation;
        core.audio_event(AudioEvent::Ended {
            generation: generation.wrapping_sub(1),
            bitrate_bps: Some(192_000),
        })
        .unwrap();
        assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
        if drain {
            let (send, receive) = bounded(1);
            core.engine.events = receive;
            send.send(AudioEvent::Ended {
                generation,
                bitrate_bps: Some(192_000),
            })
            .unwrap();
            core.finish_playback(Ok(None), false, Ok(())).unwrap();
        } else {
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: Some(192_000),
            })
            .unwrap();
        }
        let exact = core.store.track(1).unwrap().unwrap();
        assert_eq!(exact.bitrate_bps, Some(192_000));
        assert!(exact.bitrate_exact);
        let revision = core.state.library.revision;
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: Some(256_000),
        })
        .unwrap();
        assert_eq!(core.state.library.revision, revision);
        assert_eq!(core.store.track(1).unwrap().unwrap(), exact);
    }
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    started(&mut core, Some(10.0)).unwrap();
    let generation = core.generation;
    core.finish_playback(Ok(None), false, Ok(())).unwrap();
    core.audio_event(AudioEvent::Ended {
        generation,
        bitrate_bps: Some(192_000),
    })
    .unwrap();
    assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
}

#[test]
fn same_track_restart_retains_queued_started_and_eof_bitrate_upgrades() {
    for eof in [false, true] {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        let queue_id = core.queue_ids[0];
        let generation = core.generation;
        let event = if eof {
            started(&mut core, Some(10.0)).unwrap();
            AudioEvent::Ended {
                generation,
                bitrate_bps: Some(192_000),
            }
        } else {
            let mut event = started_event(&core, Some(10.0));
            if let AudioEvent::Started { info, .. } = &mut event {
                info.bitrate_bps = Some(192_000);
                info.bitrate_exact = true;
            }
            event
        };
        let _commands = recording_audio(&mut core);
        let (sender, events) = bounded(1);
        core.engine.events = events;
        sender.send(event).unwrap();

        core.start(queue_id, false).unwrap();
        assert_eq!(core.generation, generation.wrapping_add(1));
        assert!(!core.playback.as_ref().unwrap().started);
        assert!(core.playback.as_ref().unwrap().source.is_none());

        // A new decoder estimate must not replace the retiring worker's upgrade.
        let mut event = started_event(&core, Some(10.0));
        if let AudioEvent::Started { info, .. } = &mut event {
            info.bitrate_bps = Some(64_000);
        }
        for event in [None, Some(event)] {
            if let Some(event) = event {
                core.audio_event(event).unwrap();
            }
            core.publish();
            let stored = core.store.track(1).unwrap().unwrap();
            assert_eq!(stored.bitrate_bps, Some(192_000));
            assert!(stored.bitrate_exact);
            let state = core.shared.read();
            assert_eq!(state.playback.status, PlaybackStatus::Playing);
            assert_eq!(state.queue.current_id, Some(queue_id));
            assert_eq!(state.current_track().unwrap(), &stored);
            let page = core.store.queue_page(None, 0, 1).unwrap();
            assert_eq!(page.rows[0].track.bitrate_bps, stored.bitrate_bps);
            assert_eq!(page.rows[0].track.bitrate_exact, stored.bitrate_exact);
        }
    }
}

#[test]
fn playback_bitrate_runtime_replacement_and_optional_failure_do_not_interrupt_start() {
    for replace in [false, true] {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        if replace {
            let path = core.state.current_track().unwrap().path.clone();
            std::fs::write(path, b"different encoded source with different size").unwrap();
        }
        let mut event = started_event(&core, Some(10.0));
        if let AudioEvent::Started { info, .. } = &mut event {
            info.bitrate_bps = Some(if replace { 192_000 } else { u64::MAX });
            info.bitrate_exact = true;
        }
        core.audio_event(event).unwrap();
        assert!(core.playback.as_ref().unwrap().started);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        let track = core.store.track(1).unwrap().unwrap();
        assert_eq!(track.bitrate_bps, Some(128_000));
        assert!(!track.bitrate_exact);
    }
}

#[test]
fn successful_start_records_recent_once_and_twenty_percent_is_strict() {
    let (_directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.system.config).play_count_threshold_percent = 20.0;
    pending(&mut core, 1);
    assert!(core.store.history(200).unwrap().is_empty());
    started(&mut core, Some(10.0)).unwrap();
    assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
    assert_eq!(play_count(&core, 1), 0);

    // A duplicate Started must not refresh last-played a second time.
    core.store.mark_played(1, 7).unwrap();
    started(&mut core, Some(10.0)).unwrap();
    assert_eq!(core.store.history(200).unwrap()[0].played_at, 7);
    progress(&mut core, 9.0, 2.0);
    progress(&mut core, 9.0, f64::NAN);
    progress(&mut core, 9.0, f64::INFINITY);
    assert_eq!(play_count(&core, 1), 0);
    progress(&mut core, 9.0, 2.000_001);
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.state.current_track().unwrap().play_count, 1);
    assert_eq!(
        core.store.queue_page(None, 0, 1).unwrap().rows[0]
            .track
            .play_count,
        1
    );
    progress(&mut core, 9.5, 3.0);
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.state.current_track().unwrap().play_count, 1);
    assert_eq!(
        core.store.queue_page(None, 0, 1).unwrap().rows[0]
            .track
            .play_count,
        1
    );
}

#[test]
fn configured_threshold_controls_counting_including_zero_percent() {
    for percent in [0.0, 50.0, 99.0] {
        let (_directory, mut core) = fixture();
        Arc::make_mut(&mut core.state.system.config).play_count_threshold_percent = percent;
        playing(&mut core, 1);
        let boundary = 10.0 * percent / 100.0;
        progress(&mut core, 10.0, boundary);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, 10.0, boundary + 0.000_001);
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn repeated_progress_pause_seek_and_resume_count_only_once() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.store.mark_played(1, 7).unwrap();
    progress(&mut core, 1.0, 1.0);
    core.command(Command::Pause).unwrap();
    progress(&mut core, 1.0, 1.0);
    core.command(Command::Seek { seconds: 9.0 }).unwrap();
    assert_eq!(play_count(&core, 1), 0);
    core.command(Command::Resume).unwrap();
    progress(&mut core, 9.5, 1.5);
    core.command(Command::Seek { seconds: 0.0 }).unwrap();
    progress(&mut core, 0.5, 2.0);
    assert_eq!(play_count(&core, 1), 0);
    progress(&mut core, 1.0, 2.5);
    for _ in 0..3 {
        core.command(Command::Pause).unwrap();
        core.command(Command::Resume).unwrap();
        core.command(Command::Seek { seconds: 5.0 }).unwrap();
        progress(&mut core, 5.0, 2.0);
        progress(&mut core, 8.0, 8.0);
    }
    let final_activity = core.state.playback.last_heard_at.unwrap();
    core.stop().unwrap();
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(
        core.store.history(200).unwrap()[0].played_at,
        final_activity
    );
}

#[test]
fn paused_confirmations_do_not_advance_activity_at_stop() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 2.0, 2.0);
    let heard_at = core.state.playback.last_heard_at;
    core.command(Command::Pause).unwrap();
    let pause_bound = core.playback.as_ref().unwrap().paused_at.unwrap();
    progress(&mut core, 20.0, 20.0);
    assert!(core.state.playback.last_heard_at.unwrap() <= pause_bound);
    assert_eq!(core.playback.as_ref().unwrap().heard, 20.0);
    assert_eq!(play_count(&core, 1), 1);
    let generation = core.generation;
    core.finish_playback(Ok(Some((generation, 20.0))), false, Ok(()))
        .unwrap();
    assert_eq!(
        core.store.history(200).unwrap()[0].played_at,
        core.state
            .playback
            .last_heard_at
            .unwrap_or(heard_at.unwrap())
    );
}

#[test]
fn failed_load_and_stale_events_neither_count_nor_record_recent() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let generation = core.generation;
    let track = core.state.current_track().unwrap();
    core.audio_event(AudioEvent::Started {
        generation: generation.wrapping_sub(1),
        info: MediaInfo {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            duration: Some(10.0),
            codec: track.codec.clone(),
            channels: track.channels,
            sample_rate: track.sample_rate,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
        },
    })
    .unwrap();
    progress(&mut core, 10.0, 10.0);
    core.audio_event(AudioEvent::Failed {
        generation,
        message: "Decoder failed".into(),
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(
        core.state.system.last_error.as_deref(),
        Some("Decoder failed")
    );
    assert_ne!(core.generation, generation);
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.position, 0.0);
    assert_eq!(play_count(&core, 1), 0);
    assert!(core.store.history(200).unwrap().is_empty());
}

#[test]
fn final_stop_snapshot_counts_only_matching_generation_above_boundary() {
    for (heard, stale, expected) in [(2.0, false, 0), (2.01, false, 1), (10.0, true, 0)] {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        progress(&mut core, 2.0, 2.0);
        let generation = core.generation;
        let snapshot_generation = if stale { generation - 1 } else { generation };
        core.finish_playback(Ok(Some((snapshot_generation, heard))), false, Ok(()))
            .unwrap();
        assert_eq!(play_count(&core, 1), expected);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_ne!(core.generation, generation);
        assert!(core.playback.is_none());
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(play_count(&core, 1), expected);
    }
}

#[test]
fn stop_ack_records_queued_start_and_snapshot_without_terminal_navigation() {
    for (heard, expected) in [(2.0, 0), (2.01, 1)] {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        let (sender, events) = bounded(8);
        core.engine.events = events;
        sender.send(started_event(&core, Some(10.0))).unwrap();
        sender.send(started_event(&core, Some(10.0))).unwrap();
        sender
            .send(AudioEvent::Progress {
                generation,
                position_seconds: 9.0,
                listened_seconds: 2.0,
            })
            .unwrap();
        sender
            .send(AudioEvent::Progress {
                generation: generation - 1,
                position_seconds: 10.0,
                listened_seconds: 10.0,
            })
            .unwrap();
        sender
            .send(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
        sender
            .send(AudioEvent::Failed {
                generation,
                message: "Retired failure".into(),
            })
            .unwrap();
        sender
            .send(AudioEvent::DecoderStopped { generation })
            .unwrap();
        core.finish_playback(Ok(Some((generation, heard))), false, Ok(()))
            .unwrap();
        assert_eq!(play_count(&core, 1), expected);
        assert_eq!(core.store.history(200).unwrap().len(), 1);
        assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.queue.current_id, Some(1));
        assert_eq!(core.state.playback.position, 0.0);
        assert!(core.state.system.last_error.is_none());
        assert_eq!(core.generation, generation.wrapping_add(1));
        assert!(core.playback.is_none());
    }
}

#[test]
fn queued_start_rejection_fences_once_without_recursive_stop_or_count() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let generation = core.generation;
    let (sender, events) = bounded(4);
    core.engine.events = events;
    sender.send(started_event(&core, Some(10.0))).unwrap();
    sender
        .send(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
    sender
        .send(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
    core.store.remove_tracks(&[1]).unwrap();
    assert!(
        core.finish_playback(Ok(Some((generation, 10.0))), false, Ok(()))
            .is_err()
    );
    assert_eq!(core.generation, generation.wrapping_add(1));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.playback.is_none());
    while let Ok(event) = core.engine.events.try_recv() {
        core.audio_event(event).unwrap();
    }
    assert!(core.store.history(200).unwrap().is_empty());
    assert_eq!(play_count(&core, 2), 0);
}

#[test]
fn stale_events_cannot_count_or_stop_new_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let generation = core.generation;
    core.stop().unwrap();
    playing(&mut core, 2);
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    core.audio_event(AudioEvent::Ended {
        generation,
        bitrate_bps: None,
    })
    .unwrap();
    core.audio_event(AudioEvent::Failed {
        generation,
        message: "Old failure".into(),
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.state.playback.position, 0.0);
    assert!(core.state.system.last_error.is_none());
    assert_eq!(play_count(&core, 1), 0);
    assert_eq!(play_count(&core, 2), 0);
    progress(&mut core, 3.0, 3.0);
    assert_eq!(play_count(&core, 2), 1);
}

#[test]
fn started_uses_valid_decoder_duration_or_keeps_known_metadata_duration() {
    for duration in [
        None,
        Some(f64::NAN),
        Some(0.0),
        Some(f64::INFINITY),
        Some(5.0),
    ] {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        started(&mut core, duration).unwrap();
        let expected_duration = if duration == Some(5.0) { 5.0 } else { 10.0 };
        assert_eq!(core.state.playback.duration, Some(expected_duration));
        let boundary = expected_duration * 20.0 / 100.0;
        progress(&mut core, expected_duration, boundary);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, expected_duration, boundary + 0.000_001);
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn rejected_recent_update_stops_without_counting_the_start() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let generation = core.generation;
    core.store.remove_tracks(&[1]).unwrap();
    assert!(started(&mut core, Some(10.0)).is_err());
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_ne!(core.generation, generation);
    assert!(core.playback.is_none());
    assert!(core.store.history(200).unwrap().is_empty());
    assert_eq!(play_count(&core, 2), 0);
}

#[test]
fn unknown_duration_waits_for_natural_eof_and_uses_confirmed_position() {
    for (heard, expected) in [(2.0, 0), (2.01, 1), (8.0, 1)] {
        let (_directory, mut core) = fixture_with_duration(None);
        pending(&mut core, 1);
        started(&mut core, None).unwrap();
        progress(&mut core, 10.0, heard);
        assert_eq!(play_count(&core, 1), 0);
        let generation = core.generation;
        assert!(
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None
            })
            .is_err()
        );
        assert_eq!(play_count(&core, 1), expected);
        assert_eq!(core.state.playback.duration, Some(10.0));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    }
}

#[test]
fn unknown_duration_never_uses_seek_targets_heard_time_or_manual_stop() {
    let (_directory, mut core) = fixture_with_duration(None);
    pending(&mut core, 1);
    started(&mut core, None).unwrap();
    core.command(Command::Seek { seconds: 10.0 }).unwrap();
    progress(&mut core, f64::NAN, 8.0);
    let generation = core.generation;
    assert!(
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None
        })
        .is_err()
    );
    assert_eq!(play_count(&core, 1), 0);
    assert_eq!(core.state.playback.duration, None);

    pending(&mut core, 1);
    started(&mut core, None).unwrap();
    progress(&mut core, 10.0, 8.0);
    core.stop().unwrap();
    assert_eq!(play_count(&core, 1), 0);
}

#[test]
fn stopped_decoder_final_progress_counts_once_and_fences_late_events() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 2.0, 2.0);
    progress(&mut core, 2.1, 2.1);
    let generation = core.generation;
    core.audio_event(AudioEvent::DecoderStopped { generation })
        .unwrap();
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.playback.position, 0.0);
    assert!(core.playback.is_none());
    assert_eq!(play_count(&core, 1), 1);
}

#[test]
fn count_save_failure_still_stops_and_preserves_audio_error() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let playback = core.playback.as_mut().unwrap();
    playback.track_id = i64::MAX;
    playback.heard = 3.0;
    let generation = core.generation;
    let error = core
        .audio_event(AudioEvent::Failed {
            generation,
            message: "Decoder failed".into(),
        })
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("Decoder failed"));
    assert!(message.contains("does not exist"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_ne!(core.generation, generation);
}

#[test]
fn duplicate_queue_entries_count_as_separate_plays_but_share_recent_history() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 2);
    progress(&mut core, 3.0, 3.0);
    assert_eq!(play_count(&core, 2), 1);
    core.stop().unwrap();
    playing(&mut core, 3);
    progress(&mut core, 3.0, 3.0);
    progress(&mut core, 8.0, 8.0);
    assert_eq!(play_count(&core, 2), 2);
    let history = core.store.history(200).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].track_id, 2);
    assert_eq!(
        core.store.queue_track_id(2).unwrap(),
        core.store.queue_track_id(3).unwrap()
    );
    assert_ne!(core.queue_ids[1], core.queue_ids[2]);
}

#[test]
fn crossing_play_threshold_refreshes_published_history_count_during_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.publish();
    let before = core.shared.read().library.history.clone();
    progress(&mut core, 2.0, 2.0);
    core.publish();
    assert_eq!(core.shared.read().library.history[0].play_count, 0);
    progress(&mut core, 3.0, 3.0);
    core.publish();
    let published = core.shared.read().library.history.clone();
    assert_eq!(published[0].play_count, 1);
    assert_eq!(published[0].track_id, 1);
    assert_eq!(published[0].played_at, before[0].played_at);
    assert_eq!(before[0].play_count, 0);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
}
