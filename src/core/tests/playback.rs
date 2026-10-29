use super::*;
use crate::core::dispatch::BusinessEvent;

#[test]
fn published_source_generation_tracks_audio_owner_not_pause_state() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    core.publish();
    let generation = core.shared.read().source_generation;
    assert_eq!(generation, core.generation);
    core.command(Command::Pause).unwrap();
    core.publish();
    assert_eq!(core.shared.read().source_generation, generation);
    core.command(Command::Stop).unwrap();
    core.publish();
    assert_eq!(core.shared.read().source_generation, core.generation);
    assert_ne!(core.shared.read().source_generation, generation);
}

fn add_test_album(core: &mut Core) {
    core.command(Command::EditTrack {
        track_id: 1,
        title: None,
        artist: None,
        album: Some("Playback album".into()),
        decoder_hint: None,
    })
    .unwrap();
}

#[test]
fn audio_progress_publishes_album_token_only_when_statistics_change() {
    let (_directory, mut core) = fixture();
    add_test_album(&mut core);
    playing(&mut core, 1);
    core.publish();
    let before = core.shared.read().clone();
    let generation = core.generation;
    for (heard, count) in [(1.0, 0), (3.0, 1), (4.0, 1)] {
        let previous = core.shared.read().clone();
        assert!(
            core.business_event(BusinessEvent::Audio(AudioEvent::Progress {
                generation,
                position_seconds: heard,
                listened_seconds: heard,
            }))
        );
        let state = core.shared.read();
        assert_eq!(state.playback.position, heard);
        assert_eq!(state.current_track().unwrap().play_count, count);
        assert_eq!(state.library.history[0].play_count, count);
        assert_eq!(
            state.system.revision,
            previous.system.revision.wrapping_add(1)
        );
        if heard == 3.0 {
            assert_ne!(state.library.album_revision, before.library.album_revision);
        } else {
            assert_eq!(
                state.library.album_revision,
                previous.library.album_revision
            );
        }
    }
    assert!(
        core.business_event(BusinessEvent::Audio(AudioEvent::Progress {
            generation: generation.wrapping_sub(1),
            position_seconds: 9.0,
            listened_seconds: 9.0,
        }))
    );
    assert_eq!(core.shared.read().playback.position, 4.0);
    assert_eq!(play_count(&core, 1), 1);
}

#[test]
fn audio_progress_publishes_committed_statistics_without_rereading_queue_projection() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    add_test_album(&mut core);
    playing(&mut core, 1);
    core.publish();
    let before = core.shared.read().clone();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("DELETE FROM rivu_meta").unwrap();
    let generation = core.generation;
    for heard in [3.0, 4.0] {
        assert!(
            core.business_event(BusinessEvent::Audio(AudioEvent::Progress {
                generation,
                position_seconds: heard,
                listened_seconds: heard,
            }))
        );
        let state = core.shared.read();
        assert_eq!(state.playback.status, PlaybackStatus::Playing);
        assert_eq!(state.current_track().unwrap().play_count, 1);
        assert_eq!(state.library.history[0].play_count, 1);
        assert_eq!(
            state.library.revision,
            before.library.revision.wrapping_add(1)
        );
        assert_ne!(state.library.album_revision, before.library.album_revision);
        assert!(state.queue.revision > before.queue.revision);
        assert!(state.system.last_error.is_none());
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn audio_start_publishes_bitrate_revision_without_extra_shape_invalidation() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    add_test_album(&mut core);
    precision_pending(&mut core);
    core.publish();
    let before = core.shared.read().clone();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    // Removing an unrelated query target proves that publishing known metadata
    // facts needs no structural queue read.
    connection.execute_batch("DELETE FROM rivu_meta").unwrap();
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut event {
        info.bitrate_bps = Some(192_000);
        info.bitrate_exact = true;
    }
    assert!(core.business_event(BusinessEvent::Audio(event)));
    let stored = core.store.track(1).unwrap().unwrap();
    let state = core.shared.read();
    assert_eq!(stored.bitrate_bps, Some(192_000));
    assert!(stored.bitrate_exact);
    assert_eq!(
        state.current_track().unwrap().bitrate_bps,
        stored.bitrate_bps
    );
    assert!(state.current_track().unwrap().bitrate_exact);
    // The start independently changes last-played statistics exactly once;
    // bitrate precision contributes only its payload freshness revision.
    assert_eq!(
        state.library.revision,
        before.library.revision.wrapping_add(1)
    );
    assert_ne!(state.library.album_revision, before.library.album_revision);
    assert_eq!(state.queue.revision, before.queue.revision);
    assert_eq!(
        state.library.bitrate_revision,
        before.library.bitrate_revision.wrapping_add(1)
    );
    assert!(state.system.last_error.is_none());
}

#[test]
fn terminal_audio_events_publish_metadata_committed_by_retiring_started() {
    for failed in [false, true] {
        let (_directory, mut core) = fixture();
        add_test_album(&mut core);
        precision_pending(&mut core);
        core.publish();
        let before = core.shared.read().library.album_revision;
        let generation = core.generation;
        let mut event = started_event(&core, Some(10.0));
        if let AudioEvent::Started { info, .. } = &mut event {
            info.bitrate_bps = Some(192_000);
            info.bitrate_exact = true;
        }
        let (sender, events) = async_channel::bounded(1);
        core.engine.events = Some(events);
        sender.try_send(event).unwrap();
        let terminal = if failed {
            AudioEvent::Failed {
                generation,
                message: "Decoder failed".into(),
            }
        } else {
            AudioEvent::DecoderStopped { generation }
        };
        assert!(core.business_event(BusinessEvent::Audio(terminal)));
        let state = core.shared.read();
        assert_ne!(core.generation, generation);
        assert_eq!(state.playback.status, PlaybackStatus::Stopped);
        assert!(state.current_track().unwrap().bitrate_exact);
        assert_ne!(state.library.album_revision, before);
        if failed {
            assert_eq!(state.system.last_error.as_deref(), Some("Decoder failed"));
        }
    }
}

#[test]
fn deferred_bitrate_flush_refreshes_payload_without_shape_invalidation() {
    let (_directory, mut core) = fixture();
    add_test_album(&mut core);
    precision_pending(&mut core);
    started(&mut core, Some(10.0)).unwrap();
    core.publish();
    let before = core.shared.read().clone();
    core.pending_bitrate = Some((192_000, true));
    core.playback_write_deferred = true;
    core.flush_deferred_playback().unwrap();
    core.publish();
    let state = core.shared.read();
    assert!(state.current_track().unwrap().bitrate_exact);
    assert_eq!(state.current_track().unwrap().bitrate_bps, Some(192_000));
    assert_eq!(state.library.revision, before.library.revision);
    assert_eq!(
        state.library.structure_revision,
        before.library.structure_revision
    );
    assert_eq!(state.library.album_revision, before.library.album_revision);
    assert_eq!(
        state.library.playlist_revision,
        before.library.playlist_revision
    );
    assert_eq!(state.queue, before.queue);
    assert!(Arc::ptr_eq(
        &state.library.metadata_revisions,
        &before.library.metadata_revisions
    ));
    assert!(Arc::ptr_eq(
        &state.library.playlist_entry_revisions,
        &before.library.playlist_entry_revisions
    ));
    assert_eq!(
        state.revisions.changed_since(before.revisions),
        StateSections {
            playback: true,
            library: true,
            ..StateSections::default()
        }
    );
    assert_eq!(state.library.track_total, before.library.track_total);
    assert_eq!(state.library.playlist_total, before.library.playlist_total);
    assert_eq!(
        state.library.duplicate_groups_revision,
        before.library.duplicate_groups_revision
    );
    assert_eq!(
        state.library.duplicate_members_revision,
        before.library.duplicate_members_revision
    );
    assert_eq!(
        state.library.bitrate_revision,
        before.library.bitrate_revision.wrapping_add(1)
    );
    assert!(!core.playback_write_deferred);
}

#[test]
fn bitrate_commit_rejects_payload_batch_baselines_and_occurrence_continuations() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    core.enqueue(&[2]).unwrap();
    started(&mut core, Some(10.0)).unwrap();
    let playlist_id = core.store.create_playlist("Bitrate continuation").unwrap();
    core.advance_playlist_entry_revision(playlist_id);
    core.command(Command::AddPlaylist {
        playlist_id,
        track_ids: vec![1, 2],
    })
    .unwrap();
    let queue_query = Query::QueueBatch {
        query: None,
        after: None,
        limit: 1,
        expected: None,
    };
    let playlist_query = Query::PlaylistEntryBatch {
        playlist_id,
        query: None,
        after: None,
        limit: 1,
        expected: None,
    };
    let tracks_query = Query::TrackBatch {
        query: None,
        favorite: None,
        missing: None,
        after: None,
        limit: 1,
        expected: None,
    };
    let queue_before = core.query_response(&queue_query);
    let playlist_before = core.query_response(&playlist_query);
    let tracks_before = core.query_response(&tracks_query);
    let Ok(ViewResponse::QueueBatch(queue_batch)) = queue_before.result else {
        panic!("queue batch failed")
    };
    let Ok(ViewResponse::PlaylistEntryBatch(playlist_batch)) = playlist_before.result else {
        panic!("playlist batch failed")
    };
    assert!(queue_batch.next.is_some());
    assert!(playlist_batch.next.is_some());
    core.pending_bitrate = Some((192_000, true));
    core.playback_write_deferred = true;
    core.flush_deferred_playback().unwrap();
    for query in [
        Query::QueueBatch {
            query: None,
            after: queue_batch.next,
            limit: 1,
            expected: None,
        },
        Query::PlaylistEntryBatch {
            playlist_id,
            query: None,
            after: playlist_batch.next,
            limit: 1,
            expected: None,
        },
        Query::QueueBatch {
            query: None,
            after: None,
            limit: 1,
            expected: Some(queue_before.revisions),
        },
        Query::PlaylistEntryBatch {
            playlist_id,
            query: None,
            after: None,
            limit: 1,
            expected: Some(playlist_before.revisions),
        },
        Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 1,
            expected: Some(tracks_before.revisions),
        },
    ] {
        assert!(
            matches!(core.view(&query), Err(QueryError::RevisionChanged)),
            "{query:?}"
        );
    }
}

#[test]
fn audio_eof_resolved_duration_publishes_final_count_and_album_token() {
    let (_directory, mut core) = fixture_with_duration(None);
    add_test_album(&mut core);
    pending(&mut core, 1);
    started(&mut core, None).unwrap();
    progress(&mut core, 3.0, 3.0);
    core.publish();
    let before = core.shared.read().library.album_revision;
    assert_eq!(play_count(&core, 1), 0);
    let event = AudioEvent::Ended {
        generation: core.generation,
        bitrate_bps: None,
    };
    assert!(core.business_event(BusinessEvent::Audio(event)));
    let state = core.shared.read();
    assert_eq!(state.playback.duration, Some(3.0));
    assert_eq!(state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(state.current_track().unwrap().play_count, 1);
    assert_eq!(play_count(&core, 1), 1);
    assert_ne!(state.library.album_revision, before);
}

#[test]
fn rejected_audio_progress_count_preserves_statistics_and_album_token() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    playing(&mut core, 1);
    core.publish();
    let before = core.shared.read().clone();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_count BEFORE UPDATE OF play_count ON tracks
             BEGIN SELECT RAISE(ABORT,'count rejected'); END;",
        )
        .unwrap();
    let event = AudioEvent::Progress {
        generation: core.generation,
        position_seconds: 3.0,
        listened_seconds: 3.0,
    };
    assert!(core.business_event(BusinessEvent::Audio(event)));
    let state = core.shared.read();
    assert_eq!(state.playback.position, 3.0);
    assert_eq!(state.playback.status, PlaybackStatus::Playing);
    assert_eq!(state.current_track().unwrap().play_count, 0);
    assert_eq!(play_count(&core, 1), 0);
    assert!(!core.playback.as_ref().unwrap().counted);
    assert_eq!(state.library.revision, before.library.revision);
    assert_eq!(state.library.album_revision, before.library.album_revision);
    assert!(
        state
            .system
            .last_error
            .as_ref()
            .unwrap()
            .contains("count rejected")
    );
}

#[test]
fn committed_play_count_invalidates_once_without_queue_projection_reads() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    playing(&mut core, 1);
    let before_count = core.state.current_track().unwrap().play_count;
    let before = core.state.query_revisions();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("DELETE FROM rivu_meta").unwrap();
    core.playback_progress(8.0, 8.0).unwrap();
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
    // Repeated progress cannot replay a known commit or generate another token.
    assert_eq!(core.state.queue.revision, before.queue + 1);
    assert_eq!(
        core.state.current_track().unwrap().play_count,
        before_count + 1
    );
}

#[test]
fn saturated_audio_admission_preserves_state_requests_events_and_command_order() {
    let (_directory, mut core) = fixture();
    let (commands, receive) = async_channel::bounded(64);
    core.engine.replace_commands(commands.clone());
    core.command(Command::Device {
        name: Some("accepted-output".into()),
    })
    .unwrap();
    core.flush_config().unwrap();
    for index in 1..64 {
        commands
            .try_send(AudioCommand::Volume(index as f32))
            .unwrap();
    }
    let (events, event_receive) = async_channel::bounded(128);
    for index in 0..128 {
        events
            .try_send(AudioEvent::OutputSettingsFailed {
                request: core.output_settings_request,
                device: core.state.system.selected_device.clone(),
                auto_mix: core.state.configuration.config.pipewire_auto_mix,
                message: format!("accepted failure {index}"),
            })
            .unwrap();
    }
    core.engine.events = Some(event_receive);
    let saved = std::fs::read(&core.state.configuration.config_path).unwrap();
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
        std::fs::read(&core.state.configuration.config_path).unwrap(),
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
    assert_eq!(core.engine.events.as_ref().unwrap().len(), 128);
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
        let event = core.engine.events.as_mut().unwrap().try_recv().unwrap();
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
    pending(&mut core, 1);
    let revision = core.state.queue.revision;
    started(&mut core, Some(10.0)).unwrap();
    assert!(core.state.current_track().unwrap().last_played.is_some());
    assert_eq!(core.state.queue.revision, revision);
    progress(&mut core, 3.0, 3.0);
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
            records: (vec![ScanRecord {
                path: track.path,
                size: metadata.len(),
                modified_ns: metadata
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64,
                fingerprint: None,
                media,
            }])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    core.command(Command::ClearQueue).unwrap();
    core.enqueue(&[1]).unwrap();
    let queue_id = core.store.queue_id_at(0).unwrap().unwrap();
    pending(core, queue_id);
    assert!(core.playback.as_ref().unwrap().source.is_some());
}

#[test]
fn playback_started_upgrades_estimate_and_duplicate_cannot_downgrade() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    let playlist_id = core.store.create_playlist("Bitrate scope").unwrap();
    core.advance_playlist_entry_revision(playlist_id);
    core.command(Command::AddPlaylist {
        playlist_id,
        track_ids: vec![1, 2],
    })
    .unwrap();
    let query = Query::PlaylistEntries {
        playlist_id,
        query: None,
        offset: 0,
        limit: 2,
    };
    let playlist_stamp = query.stamp(core.query_response(&query).revisions);
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut event {
        info.bitrate_bps = Some(128_000);
        info.bitrate_exact = true;
    }
    core.audio_event(event).unwrap();
    assert!(core.store.track(1).unwrap().unwrap().bitrate_exact);
    assert!(core.state.current_track().unwrap().bitrate_exact);
    assert!(
        core.store.queue_page(None, 0, 10).unwrap().rows[0]
            .track
            .bitrate_exact
    );
    assert_ne!(
        query.stamp(core.query_response(&query).revisions),
        playlist_stamp
    );
    let revision = core.state.library.revision;
    let bitrate_revision = core.state.library.bitrate_revision;
    let mut duplicate = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut duplicate {
        info.bitrate_bps = Some(64_000);
    }
    core.audio_event(duplicate).unwrap();
    assert_eq!(core.state.library.revision, revision);
    assert_eq!(core.state.library.bitrate_revision, bitrate_revision);
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
    let bitrate_revision = core.state.library.bitrate_revision;
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { generation, info } = &mut event {
        *generation = generation.wrapping_sub(1);
        info.bitrate_bps = Some(192_000);
        info.bitrate_exact = true;
    }
    core.audio_event(event).unwrap();
    assert_eq!(core.state.library.revision, revision);
    assert_eq!(core.state.library.bitrate_revision, bitrate_revision);
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
            let (send, receive) = async_channel::bounded(1);
            core.engine.events = Some(receive);
            send.try_send(AudioEvent::Ended {
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
fn retiring_eof_bitrate_remains_fresh_after_final_statistics() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    started(&mut core, Some(10.0)).unwrap();
    let generation = core.generation;
    let baseline = core.state.library.bitrate_revision;
    let (send, receive) = async_channel::bounded(1);
    core.engine.events = Some(receive);
    send.try_send(AudioEvent::Ended {
        generation,
        bitrate_bps: Some(192_000),
    })
    .unwrap();
    core.finish_playback(
        Ok(Some(audio::AudioSnapshot {
            generation,
            position_seconds: 10.0,
            listened_seconds: 3.0,
        })),
        true,
        Ok(()),
    )
    .unwrap();
    assert_eq!(play_count(&core, 1), 1);
    let library = &core.state.library;
    assert_eq!(library.bitrate_revision, baseline.wrapping_add(1));
    let track = core.store.track(1).unwrap().unwrap();
    assert_eq!(
        (track.bitrate_bps, track.bitrate_exact),
        (Some(192_000), true)
    );
}

#[test]
fn same_track_restart_retains_queued_started_and_eof_bitrate_upgrades() {
    for eof in [false, true] {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        let queue_id = core.store.queue_id_at(0).unwrap().unwrap();
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
        let (sender, events) = async_channel::bounded(1);
        core.engine.events = Some(events);
        sender.try_send(event).unwrap();

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
        let bitrate_revision = core.state.library.bitrate_revision;
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
        assert_eq!(core.state.library.bitrate_revision, bitrate_revision);
        assert!(core.playback.as_ref().unwrap().started);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        let track = core.store.track(1).unwrap().unwrap();
        assert_eq!(track.bitrate_bps, Some(128_000));
        assert!(!track.bitrate_exact);
    }
}

#[test]
fn failed_bitrate_transaction_does_not_publish_an_update() {
    let (directory, mut core) = fixture();
    precision_pending(&mut core);
    let before = core.state.library.bitrate_revision;
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_bitrate BEFORE UPDATE OF bitrate_bps,bitrate_exact ON tracks
         BEGIN SELECT RAISE(ABORT,'reject bitrate enrichment'); END;",
        )
        .unwrap();
    let mut event = started_event(&core, Some(10.0));
    if let AudioEvent::Started { info, .. } = &mut event {
        info.bitrate_bps = Some(192_000);
        info.bitrate_exact = true;
    }
    core.audio_event(event).unwrap();
    assert_eq!(core.state.library.bitrate_revision, before);
    assert!(core.playback.as_ref().unwrap().started);
    let stored = core.store.track(1).unwrap().unwrap();
    assert_eq!(
        (stored.bitrate_bps, stored.bitrate_exact),
        (Some(128_000), false)
    );
    let current = core.state.current_track().unwrap();
    assert_eq!(
        (current.bitrate_bps, current.bitrate_exact),
        (Some(128_000), false)
    );
}

#[test]
fn unchanged_bitrate_enrichment_does_not_publish_an_update() {
    let (_directory, mut core) = fixture();
    precision_pending(&mut core);
    started(&mut core, Some(10.0)).unwrap();
    let before = core.state.library.bitrate_revision;
    core.pending_bitrate = Some((128_000, false));
    core.playback_write_deferred = true;
    core.flush_deferred_playback().unwrap();
    assert_eq!(core.state.library.bitrate_revision, before);
    let stored = core.store.track(1).unwrap().unwrap();
    assert_eq!(
        (stored.bitrate_bps, stored.bitrate_exact),
        (Some(128_000), false)
    );
}

#[test]
fn successful_start_records_recent_once_and_twenty_percent_is_strict() {
    let (_directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.configuration.config).play_count_threshold_percent = 20.0;
    pending(&mut core, 1);
    assert!(core.store.history(200).unwrap().is_empty());
    started(&mut core, Some(10.0)).unwrap();
    assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
    assert_eq!(play_count(&core, 1), 0);

    // A duplicate Started must not refresh last-played a second time.
    let played_at = core.store.history(200).unwrap()[0].played_at;
    core.playback.as_mut().unwrap().paused_at = Some(7);
    started(&mut core, Some(10.0)).unwrap();
    assert_eq!(core.store.history(200).unwrap()[0].played_at, played_at);
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
        Arc::make_mut(&mut core.state.configuration.config).play_count_threshold_percent = percent;
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
    core.finish_playback(
        Ok(Some(rivu_audio::AudioSnapshot {
            generation,
            position_seconds: 20.0,
            listened_seconds: 20.0,
        })),
        false,
        Ok(()),
    )
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
        core.finish_playback(
            Ok(Some(audio::AudioSnapshot {
                generation: snapshot_generation,
                position_seconds: 2.0,
                listened_seconds: heard,
            })),
            false,
            Ok(()),
        )
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
fn switching_selection_counts_final_snapshot_without_invalidating_its_position() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 2.0, 2.0);
    let generation = core.generation;
    let (commands, receive) = async_channel::bounded(4);
    let idle_commands = core.engine.replace_commands(commands);
    let responder = thread::spawn(move || {
        let AudioCommand::StopAndSnapshot(reply) = receive.recv_blocking().unwrap() else {
            panic!("selection must retire the previous playback");
        };
        reply
            .send(audio::AudioSnapshot {
                generation,
                position_seconds: 2.01,
                listened_seconds: 2.01,
            })
            .unwrap();
    });

    core.start_with_status(2, true, PlaybackStatus::Stopped)
        .unwrap();
    core.engine.replace_commands(idle_commands);
    responder.join().unwrap();
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.state.queue.current_id, Some(2));
    assert_eq!(core.state.queue.current_position, Some(1));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_queue_saved(&core);
    core.audio_event(AudioEvent::Progress {
        generation,
        position_seconds: 10.0,
        listened_seconds: 10.0,
    })
    .unwrap();
    assert_eq!(play_count(&core, 1), 1);
}

#[test]
fn stop_ack_records_queued_start_and_snapshot_without_terminal_navigation() {
    for (heard, expected) in [(2.0, 0), (2.01, 1)] {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        let (sender, events) = async_channel::bounded(8);
        core.engine.events = Some(events);
        sender.try_send(started_event(&core, Some(10.0))).unwrap();
        sender.try_send(started_event(&core, Some(10.0))).unwrap();
        sender
            .try_send(AudioEvent::Progress {
                generation,
                position_seconds: 9.0,
                listened_seconds: 2.0,
            })
            .unwrap();
        sender
            .try_send(AudioEvent::Progress {
                generation: generation - 1,
                position_seconds: 10.0,
                listened_seconds: 10.0,
            })
            .unwrap();
        sender
            .try_send(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
        sender
            .try_send(AudioEvent::Failed {
                generation,
                message: "Retired failure".into(),
            })
            .unwrap();
        sender
            .try_send(AudioEvent::DecoderStopped { generation })
            .unwrap();
        core.finish_playback(
            Ok(Some(rivu_audio::AudioSnapshot {
                generation,
                position_seconds: heard,
                listened_seconds: heard,
            })),
            false,
            Ok(()),
        )
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
    let (directory, mut core) = fixture_with_database(Some(10.0));
    pending(&mut core, 1);
    let generation = core.generation;
    let (sender, events) = async_channel::bounded(4);
    core.engine.events = Some(events);
    sender.try_send(started_event(&core, Some(10.0))).unwrap();
    sender
        .try_send(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
    sender
        .try_send(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_start_activity BEFORE UPDATE OF last_played ON tracks BEGIN SELECT RAISE(ABORT,'activity rejected'); END;").unwrap();
    assert!(
        core.finish_playback(
            Ok(Some(rivu_audio::AudioSnapshot {
                generation,
                position_seconds: 10.0,
                listened_seconds: 10.0
            })),
            false,
            Ok(())
        )
        .is_err()
    );
    assert_eq!(core.generation, generation.wrapping_add(1));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.playback.is_none());
    while let Ok(event) = core.engine.events.as_mut().unwrap().try_recv() {
        core.audio_event(event).unwrap();
    }
    assert!(core.store.history(200).unwrap().is_empty());
    assert_eq!(play_count(&core, 1), 0);
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
    let (directory, mut core) = fixture_with_database(Some(10.0));
    pending(&mut core, 1);
    let generation = core.generation;
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_start_activity BEFORE UPDATE OF last_played ON tracks BEGIN SELECT RAISE(ABORT,'activity rejected'); END;").unwrap();
    assert!(started(&mut core, Some(10.0)).is_err());
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_ne!(core.generation, generation);
    assert!(core.playback.is_none());
    assert!(core.store.history(200).unwrap().is_empty());
    assert_eq!(play_count(&core, 1), 0);
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
    assert_ne!(
        core.store.queue_id_at(1).unwrap().unwrap(),
        core.store.queue_id_at(2).unwrap().unwrap()
    );
}

#[test]
fn unchanged_activity_skips_history_but_failed_real_refresh_retries() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    pending(&mut core, 1);
    core.playback.as_mut().unwrap().paused_at = Some(10);
    core.store.mark_played(1, 10).unwrap();
    core.replace_history(core.store.history(MAX_QUERY_ROWS).unwrap());
    let history = Arc::clone(&core.state.library.history);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    // An unrelated unreadable history row proves no history SELECT occurs on
    // either the immediate or deferred no-op activity path.
    connection
        .execute("UPDATE tracks SET last_played='unreadable' WHERE id=4", [])
        .unwrap();
    started(&mut core, Some(10.0)).unwrap();
    assert!(Arc::ptr_eq(&history, &core.state.library.history));
    core.playback_write_deferred = true;
    core.flush_deferred_playback().unwrap();
    assert!(Arc::ptr_eq(&history, &core.state.library.history));
    assert!(!core.playback_history_pending);

    core.playback.as_mut().unwrap().last_heard_at = Some(11);
    core.playback_write_deferred = true;
    assert!(core.flush_deferred_playback().is_err());
    assert_eq!(core.store.track(1).unwrap().unwrap().last_played, Some(11));
    assert!(core.playback_history_pending);
    assert!(Arc::ptr_eq(&history, &core.state.library.history));
    connection
        .execute("UPDATE tracks SET last_played=NULL WHERE id=4", [])
        .unwrap();
    // The retry's mark_played is now unchanged, but the failed projection fact
    // remains pending until an authoritative history read succeeds.
    core.playback_write_deferred = true;
    core.flush_deferred_playback().unwrap();
    assert!(!core.playback_history_pending);
    assert!(!Arc::ptr_eq(&history, &core.state.library.history));
    assert_eq!(core.state.library.history[0].played_at, 11);
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

#[test]
fn ordinary_stop_retains_failed_statistics_without_recounting_committed_plays() {
    for failure in ["activity", "count", "history"] {
        let (directory, mut core) = fixture_with_database(Some(10.0));
        playing(&mut core, 1);
        core.playback.as_mut().unwrap().paused_at = Some(17);
        core.playback.as_mut().unwrap().last_heard_at = Some(17);
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection.execute_batch(match failure {
            "activity" => "CREATE TRIGGER reject_stop_activity BEFORE UPDATE OF last_played ON tracks BEGIN SELECT RAISE(FAIL,'activity unavailable'); END;",
            "count" => "CREATE TRIGGER reject_stop_count BEFORE UPDATE OF play_count ON tracks BEGIN SELECT RAISE(FAIL,'count unavailable'); END;",
            "history" => "CREATE TRIGGER reject_stop_projection AFTER UPDATE OF last_played ON tracks WHEN NEW.id=1 BEGIN UPDATE tracks SET last_played='unreadable' WHERE id=4; END;",
            _ => unreachable!(),
        }).unwrap();
        let generation = core.generation;
        assert!(
            core.finish_playback(
                Ok(Some(audio::AudioSnapshot {
                    generation,
                    position_seconds: 3.0,
                    listened_seconds: 3.0
                })),
                false,
                Ok(()),
            )
            .is_err()
        );
        assert_eq!(core.generation, generation.wrapping_add(1));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert!(core.playback.is_none());
        let retired = core.retired_playback.as_ref().unwrap();
        assert_eq!(retired.stats.heard, 3.0);
        assert_eq!(retired.stats.counted, failure != "count");
        assert_eq!(play_count(&core, 1), u64::from(failure != "count"));
        if failure == "count" {
            // An independent activity commit is not skipped by the count failure.
            assert_eq!(core.store.track(1).unwrap().unwrap().last_played, Some(17));
        }
        assert!(
            core.start_with_status(2, false, PlaybackStatus::Stopped)
                .is_err()
        );
        assert!(core.playback.is_none());
        assert!(core.retired_playback.is_some());
        core.command(Command::Volume { value: 0.37 }).unwrap();
        connection
            .execute_batch(match failure {
                "activity" => "DROP TRIGGER reject_stop_activity;",
                "count" => "DROP TRIGGER reject_stop_count;",
                "history" => "DROP TRIGGER reject_stop_projection; UPDATE tracks SET last_played=NULL WHERE id=4;",
                _ => unreachable!(),
            })
            .unwrap();
        core.flush_deferred_playback().unwrap();
        assert!(core.retired_playback.is_none());
        assert!(core.playback.is_none());
        assert_eq!(play_count(&core, 1), 1);
        let history = core.store.history(MAX_QUERY_ROWS).unwrap();
        assert_eq!(history[0].played_at, 17);
        assert_eq!(history[0].play_count, 1);
        assert_eq!(core.state.library.history[0].played_at, 17);
        core.flush_deferred_playback().unwrap();
        assert_eq!(play_count(&core, 1), 1);
        core.start_with_status(2, false, PlaybackStatus::Stopped)
            .unwrap();
        assert_eq!(core.state.current_track().unwrap().id, 2);
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn stop_bookkeeping_error_does_not_skip_final_confirmed_statistics() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let generation = core.generation;
    let error = core
        .finish_playback(
            Ok(Some(audio::AudioSnapshot {
                generation,
                position_seconds: 3.0,
                listened_seconds: 3.0,
            })),
            false,
            Err(anyhow::anyhow!("Earlier event projection failed")),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "Earlier event projection failed");
    assert_eq!(play_count(&core, 1), 1);
    assert!(core.retired_playback.is_none());
    assert!(core.playback.is_none());
    assert_eq!(core.store.history(MAX_QUERY_ROWS).unwrap()[0].play_count, 1);
}

#[test]
fn stop_retains_later_progress_after_queued_start_projection_failure() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    pending(&mut core, 1);
    let generation = core.generation;
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch("CREATE TRIGGER reject_stop_projection AFTER UPDATE OF last_played ON tracks WHEN NEW.id=1 BEGIN UPDATE tracks SET last_played='unreadable' WHERE id=4; END;")
        .unwrap();
    let (sender, events) = async_channel::bounded(4);
    core.engine.events = Some(events);
    sender.try_send(started_event(&core, Some(10.0))).unwrap();
    for heard in [3.0, 8.0] {
        sender
            .try_send(AudioEvent::Progress {
                generation,
                position_seconds: heard,
                listened_seconds: heard,
            })
            .unwrap();
    }
    assert!(core.finish_playback(Ok(None), false, Ok(())).is_err());
    let retired = core.retired_playback.as_ref().unwrap();
    assert_eq!(retired.stats.heard, 8.0);
    assert!(retired.stats.counted);
    assert_eq!(play_count(&core, 1), 1);
    connection
        .execute_batch(
            "DROP TRIGGER reject_stop_projection; UPDATE tracks SET last_played=NULL WHERE id=4;",
        )
        .unwrap();
    core.flush_deferred_playback().unwrap();
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.state.library.history[0].play_count, 1);
    assert!(core.retired_playback.is_none());
}

#[test]
fn stop_snapshot_failure_still_saves_previously_confirmed_activity() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 3.0, 3.0);
    core.playback.as_mut().unwrap().last_heard_at = Some(17);
    let error = core
        .finish_playback(
            Err(anyhow::anyhow!("Output stop unavailable")),
            false,
            Ok(()),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "Output stop unavailable");
    assert_eq!(core.store.history(MAX_QUERY_ROWS).unwrap()[0].played_at, 17);
    assert_eq!(play_count(&core, 1), 1);
    assert!(core.retired_playback.is_none());
    assert!(core.playback.is_none());
}
