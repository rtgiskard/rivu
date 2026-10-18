use super::*;

#[test]
fn committed_track_edit_invalidates_before_failed_refresh_and_same_edit_is_noop() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    core.state.queue.current_id = Some(1);
    core.state.current_track = Some(Arc::new(core.track(1).unwrap()));
    core.save_queue().unwrap();
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    // SQLite accepts text in this INTEGER-affinity column; Track decoding does not.
    connection
        .execute_batch(
            "CREATE TRIGGER fail_edit_refresh AFTER UPDATE OF title_override ON tracks
         WHEN NEW.id=1 BEGIN UPDATE tracks SET channels='unreadable' WHERE id=NEW.id; END;",
        )
        .unwrap();
    let edit = || Command::EditTrack {
        track_id: 1,
        title: Some("Committed title".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    };
    let before = core.state.query_revisions();
    let error = core.command(edit()).unwrap_err();
    assert!(error.to_string().contains("Track edit committed"));
    let title: String = connection
        .query_row("SELECT title_override FROM tracks WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(title, "Committed title");
    let committed = core.state.query_revisions();
    assert_eq!(committed.library, before.library + 1);
    assert_eq!(committed.structure, before.structure);
    assert_eq!(committed.playlist, before.playlist);
    assert!(committed.queue > before.queue);
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
    assert!(matches!(
        core.view(&Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 1,
            expected: Some(before),
        }),
        Err(QueryError::RevisionChanged)
    ));
    // A retry cannot replay a committed change or disguise it as a new mutation.
    core.command(edit()).unwrap();
    assert_eq!(core.state.query_revisions(), committed);
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
    connection
        .execute_batch("DROP TRIGGER fail_edit_refresh; UPDATE tracks SET channels=2 WHERE id=1;")
        .unwrap();
    core.refresh_queue_tracks().unwrap();
    assert_eq!(core.state.current_track().unwrap().title, "Committed title");
    assert_eq!(core.state.query_revisions(), committed);
}

#[test]
fn failed_edits_and_empty_deletions_do_not_invalidate() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_edit BEFORE UPDATE OF title_override ON tracks
         BEGIN SELECT RAISE(ABORT,'edit rejected'); END;",
        )
        .unwrap();
    let before = core.state.query_revisions();
    let error = core
        .command(Command::EditTrack {
            track_id: 1,
            title: Some("Not committed".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("edit rejected"));
    assert!(!error.to_string().contains("committed"));
    assert_eq!(core.track(1).unwrap().title, "Track 1");
    assert_eq!(core.state.query_revisions(), before);
    core.command(Command::RemoveTracks {
        track_ids: Vec::new(),
    })
    .unwrap();
    core.command(Command::RemoveMissingTracks).unwrap();
    assert_eq!(core.state.query_revisions(), before);
}

#[test]
fn committed_track_deletions_invalidate_before_failed_refresh() {
    for missing in [false, true] {
        let (directory, mut core) = fixture_with_database(Some(10.0), true);
        if missing {
            let path = core.track(2).unwrap().path;
            let impacts = core
                .store
                .apply_scan(&ScanResult {
                    roots: vec![path],
                    ..ScanResult::default()
                })
                .unwrap();
            core.apply_scan_impacts(impacts).unwrap();
        }
        core.command(Command::CreatePlaylistWithTracks {
            name: "Affected".into(),
            track_ids: vec![2, 3],
        })
        .unwrap();
        core.command(Command::CreatePlaylistWithTracks {
            name: "Also affected".into(),
            track_ids: vec![2, 4],
        })
        .unwrap();
        core.command(Command::CreatePlaylistWithTracks {
            name: "Unchanged".into(),
            track_ids: vec![1, 3],
        })
        .unwrap();
        let affected = core.state.playlist_query_revisions(1);
        let also_affected = core.state.playlist_query_revisions(2);
        let unchanged = core.state.playlist_query_revisions(3);
        // The injected change targets a surviving row, not a deletion-cache entry.
        core.store.set_track_cache_page_size(0);
        core.state.queue.current_id = Some(1);
        core.state.current_track = Some(Arc::new(core.track(1).unwrap()));
        core.save_queue().unwrap();
        let current = Arc::clone(core.state.current_track.as_ref().unwrap());
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_delete_refresh AFTER DELETE ON tracks WHEN OLD.id=2
             BEGIN UPDATE tracks SET channels='unreadable' WHERE id=1; END;",
            )
            .unwrap();
        let before = core.state.query_revisions();
        let command = if missing {
            Command::RemoveMissingTracks
        } else {
            Command::RemoveTracks { track_ids: vec![2] }
        };
        let error = core.command(command).unwrap_err();
        assert!(error.to_string().contains("Track deletion committed"));
        assert!(core.store.track(2).unwrap().is_none());
        let committed = core.state.query_revisions();
        assert_eq!(committed.library, before.library + 1);
        assert_eq!(committed.structure, before.structure + 1);
        assert_eq!(committed.playlist, before.playlist + 1);
        assert!(committed.queue > before.queue);
        assert_ne!(
            core.state.playlist_query_revisions(1).playlist_entries,
            affected.playlist_entries
        );
        assert_ne!(
            core.state.playlist_query_revisions(2).playlist_entries,
            also_affected.playlist_entries
        );
        assert_eq!(
            core.state.playlist_query_revisions(3).playlist_entries,
            unchanged.playlist_entries
        );
        assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [3]);
        assert_eq!(core.store.playlist_track_ids(2, 10).unwrap(), [4]);
        assert_eq!(core.store.playlist_track_ids(3, 10).unwrap(), [1, 3]);
        assert_eq!(core.state.library.track_total, 3);
        assert_eq!(core.queue_ids, [1, 4, 5]);
        assert!(Arc::ptr_eq(
            core.state.current_track.as_ref().unwrap(),
            &current
        ));
        core.command(Command::RemoveMissingTracks).unwrap();
        assert_eq!(core.state.query_revisions(), committed);
    }
}

#[test]
fn metadata_intents_merge_and_pages_invalidate_only_for_queued_changes() {
    for title_first in [true, false] {
        let (_directory, mut core) = fixture();
        core.store.set_queue_current(Some(1)).unwrap();
        core.refresh_queue_tracks().unwrap();
        let title = Command::EditTrack {
            track_id: 1,
            title: Some("Title".into()),
            artist: None,
            album: None,
            decoder_hint: Some(crate::model::DecoderHint::Ffmpeg),
        };
        let artist = Command::EditTrack {
            track_id: 1,
            title: None,
            artist: Some("Artist".into()),
            album: None,
            decoder_hint: None,
        };
        let revision = core.state.queue.revision;
        for command in if title_first {
            [title, artist]
        } else {
            [artist, title]
        } {
            core.command(command).unwrap();
        }
        assert_ne!(core.state.queue.revision, revision);
        let current = core.state.current_track().unwrap();
        assert_eq!((&*current.title, &*current.artist), ("Title", "Artist"));
        assert_eq!(current.decoder_hint, crate::model::DecoderHint::Ffmpeg);
        let page = core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap();
        assert_eq!(
            (&*page.rows[0].track.title, &*page.rows[0].track.artist),
            ("Title", "Artist")
        );
        core.remove_queue_entries(&[5]).unwrap();
        core.publish();
        let before = core.state.revisions;
        let queue = core.state.queue.clone();
        core.command(Command::EditTrack {
            track_id: 4,
            title: Some("Unqueued".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        core.publish();
        assert_eq!(core.state.queue, queue);
        assert_eq!(
            core.state.revisions.changed_since(before),
            StateSections {
                library: true,
                ..StateSections::default()
            }
        );
        core.command(Command::SetFavorite {
            track_ids: vec![2, 2, 3],
            favorite: true,
        })
        .unwrap();
        let rows = core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap().rows;
        assert!(rows[1].track.favorite && rows[2].track.favorite && rows[3].track.favorite);
        assert!(!rows[0].track.favorite);
    }
}

#[test]
fn unchanged_metadata_favorite_and_playlist_intents_preserve_revisions() {
    let (_directory, mut core) = fixture();
    core.store.set_queue_current(Some(1)).unwrap();
    core.refresh_queue_tracks().unwrap();
    core.command(Command::CreatePlaylist { name: "Mix".into() })
        .unwrap();
    // Creating an equal-value override changes member facts, not display payload.
    let before = core.state.query_revisions();
    let before_current = Arc::clone(core.state.current_track.as_ref().unwrap());
    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("Track 1".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    let created = core.state.query_revisions();
    assert_ne!(created.duplicate_members, before.duplicate_members);
    assert_eq!(
        QueryRevisions {
            duplicate_members: before.duplicate_members,
            ..created
        },
        before
    );
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &before_current
    ));
    core.publish();
    let revisions = core.state.query_revisions();
    let sections = core.state.revisions;
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    for command in [
        Command::EditTrack {
            track_id: 1,
            title: None,
            artist: None,
            album: None,
            decoder_hint: None,
        },
        Command::EditTrack {
            track_id: 1,
            title: Some("Track 1".into()),
            artist: None,
            album: None,
            decoder_hint: Some(DecoderHint::Auto),
        },
        Command::SetFavorite {
            track_ids: vec![1, 1, 2],
            favorite: false,
        },
        Command::SetFavorite {
            track_ids: vec![],
            favorite: true,
        },
        Command::RenamePlaylist {
            playlist_id: 1,
            name: "Mix".into(),
        },
    ] {
        core.command(command).unwrap();
        core.publish();
        assert_eq!(core.state.query_revisions(), revisions);
        assert_eq!(core.state.revisions, sections);
        assert!(Arc::ptr_eq(
            core.state.current_track.as_ref().unwrap(),
            &current
        ));
    }
    for command in [
        Command::EditTrack {
            track_id: 999,
            title: None,
            artist: None,
            album: None,
            decoder_hint: None,
        },
        Command::SetFavorite {
            track_ids: vec![1, 999],
            favorite: true,
        },
        Command::RenamePlaylist {
            playlist_id: 999,
            name: "Mix".into(),
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.query_revisions(), revisions);
        assert!(!core.track(1).unwrap().favorite);
    }
}

#[test]
fn decoder_hint_intent_refreshes_current_without_invalidating_queue_or_restarting_audio() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 3);
    core.publish();
    let revisions = core.state.query_revisions();
    let sections = core.state.revisions;
    let playback = core.state.playback.clone();
    let generation = core.generation;
    for hint in [DecoderHint::Ffmpeg, DecoderHint::Native, DecoderHint::Auto] {
        core.command(Command::EditTrack {
            track_id: 2,
            title: None,
            artist: None,
            album: None,
            decoder_hint: Some(hint),
        })
        .unwrap();
        core.publish();
        assert_eq!(core.state.current_track().unwrap().decoder_hint, hint);
        assert_eq!(core.track(2).unwrap().decoder_hint, hint);
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.state.queue.revision, revisions.queue);
        assert_eq!(core.state.playback, playback);
        assert_eq!(core.generation, generation);
        assert!(core.playback.is_some());
        let query = Query::QueueBatch {
            query: None,
            after: None,
            limit: 5,
            expected: Some(revisions),
        };
        assert!(matches!(core.view(&query), Ok(ViewResponse::QueueBatch(_))));
        let before_repeat = core.state.revisions;
        core.command(Command::EditTrack {
            track_id: 2,
            title: None,
            artist: None,
            album: None,
            decoder_hint: Some(hint),
        })
        .unwrap();
        core.publish();
        assert_eq!(core.state.revisions, before_repeat);
    }
    assert_ne!(core.state.library.revision, revisions.library);
    assert_ne!(core.state.revisions.playback, sections.playback);
    assert_eq!(core.state.revisions.queue, sections.queue);
}

#[test]
fn targeted_metadata_preserves_unrelated_track_and_playlist_reads() {
    let (_directory, mut core) = fixture();
    for (name, track_ids) in [("one", vec![1, 2]), ("two", vec![3, 4])] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids,
        })
        .unwrap();
    }
    let track = Query::Track { track_id: 3 };
    let track_stamp = track.stamp(core.query_response(&track).revisions);
    let entries = Query::PlaylistEntries {
        query: None,
        playlist_id: 2,
        offset: 0,
        limit: 1,
    };
    let baseline = core.query_response(&entries).revisions;
    let batch = Query::PlaylistEntryBatch {
        query: None,
        playlist_id: 2,
        after: None,
        limit: 1,
        expected: Some(baseline),
    };
    for command in [
        Command::EditTrack {
            track_id: 1,
            title: Some("new title".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        },
        Command::SetFavorite {
            track_ids: vec![1, 1],
            favorite: true,
        },
        Command::ToggleFavorite { track_ids: vec![1] },
    ] {
        core.command(command).unwrap();
        assert_eq!(
            track.stamp(core.query_response(&track).revisions),
            track_stamp
        );
        let ViewResponse::PlaylistEntryBatch(page) = core.view(&batch).unwrap() else {
            panic!("expected playlist batch")
        };
        assert_eq!(page.rows[0].track_id, 3);
    }
    let related = Query::PlaylistEntries {
        query: None,
        playlist_id: 1,
        offset: 0,
        limit: 1,
    };
    let before_decoder = core.query_response(&related).revisions;
    let edited = Query::Track { track_id: 1 };
    let edited_before = edited.stamp(core.query_response(&edited).revisions);
    core.command(Command::EditTrack {
        track_id: 1,
        title: None,
        artist: None,
        album: None,
        decoder_hint: Some(crate::model::DecoderHint::Native),
    })
    .unwrap();
    assert_eq!(
        related.stamp(before_decoder),
        related.stamp(core.query_response(&related).revisions)
    );
    assert_ne!(
        edited_before,
        edited.stamp(core.query_response(&edited).revisions)
    );
    let before_failure = core.state.query_revisions();
    assert!(
        core.command(Command::SetFavorite {
            track_ids: vec![1, -1],
            favorite: true
        })
        .is_err()
    );
    assert_eq!(before_failure, core.state.query_revisions());
    assert_eq!(
        track.stamp(core.query_response(&track).revisions),
        track_stamp
    );
}

#[test]
fn favorite_updates_preserve_counts_and_recent_history() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 3.0, 3.0);
    let played_at = core.store.history(200).unwrap()[0].played_at;
    core.command(Command::SetFavorite {
        track_ids: vec![1, 2],
        favorite: true,
    })
    .unwrap();
    assert!(core.track(1).unwrap().favorite);
    assert!(core.track(2).unwrap().favorite);
    assert!(!core.track(3).unwrap().favorite);
    assert!(core.store.history(200).unwrap()[0].favorite);
    assert!(core.state.library.history[0].favorite);
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: false,
    })
    .unwrap();
    assert!(!core.track(1).unwrap().favorite);
    assert!(core.track(2).unwrap().favorite);
    assert!(!core.store.history(200).unwrap()[0].favorite);
    assert!(!core.state.library.history[0].favorite);
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.store.history(200).unwrap()[0].played_at, played_at);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
}

#[test]
fn favorite_toggle_inverts_mixed_tracks_once_and_rejects_invalid_groups() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    progress(&mut core, 3.0, 3.0);
    core.command(Command::SetFavorite {
        track_ids: vec![2],
        favorite: true,
    })
    .unwrap();
    let playback = core.state.playback.clone();
    let played_at = core.store.history(200).unwrap()[0].played_at;
    let revisions = core.state.query_revisions();
    for track_ids in [vec![], vec![1, 999]] {
        let empty = track_ids.is_empty();
        assert_eq!(
            core.command(Command::ToggleFavorite { track_ids }).is_ok(),
            empty
        );
        assert_eq!(core.state.query_revisions(), revisions);
        assert!(!core.track(1).unwrap().favorite);
        assert!(core.track(2).unwrap().favorite);
    }
    for (first, second) in [(true, false), (false, true)] {
        core.command(Command::ToggleFavorite {
            track_ids: vec![1, 2, 1],
        })
        .unwrap();
        assert_eq!(core.track(1).unwrap().favorite, first);
        assert_eq!(core.track(2).unwrap().favorite, second);
        assert!(!core.track(3).unwrap().favorite);
        assert_eq!(core.state.current_track().unwrap().favorite, first);
        assert_eq!(core.state.library.history[0].favorite, first);
        let history = core.store.history(200).unwrap();
        assert_eq!(history[0].favorite, first);
        assert_eq!(history[0].played_at, played_at);
        for row in core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap().rows {
            let expected = match row.track_id {
                1 => first,
                2 => second,
                _ => false,
            };
            assert_eq!(row.track.favorite, expected);
        }
        assert_ne!(core.state.query_revisions().library, revisions.library);
        assert_ne!(core.state.query_revisions().queue, revisions.queue);
        assert_eq!(core.state.playback, playback);
        assert_eq!(play_count(&core, 1), 1);
    }
}

#[test]
fn removing_missing_tracks_reuses_queue_playlist_and_recent_cleanup_without_disk_deletes() {
    let (_directory, mut core) = fixture();
    let kept_path = core.track(1).unwrap().path.clone();
    std::fs::write(&kept_path, b"Do not delete").unwrap();
    let playlist_id = core.store.create_playlist("Mixed availability").unwrap();
    core.store.add_playlist(playlist_id, &[1, 2]).unwrap();
    core.playlist_created(playlist_id, true).unwrap();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            roots: vec![core.track(2).unwrap().path.clone()],
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    playing(&mut core, 2);
    core.played = vec![1, 2, 3];
    core.played_cursor = 2;
    core.command(Command::RemoveMissingTracks).unwrap();
    assert_eq!(
        core.store
            .library_view_page(None, None, None, LibrarySort::Id, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        [1, 3, 4]
    );
    assert_eq!(core.queue_ids, [1, 4, 5]);
    assert_eq!(
        core.store
            .playlist_entries_page(playlist_id, None, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(core.played, [1]);
    assert_eq!(core.played_cursor, 1);
    assert_eq!(core.state.queue.current_id, None);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.state.library.history.is_empty());
    assert_eq!(std::fs::read(&kept_path).unwrap(), b"Do not delete");
}

#[test]
fn track_removal_validates_the_set_before_stopping_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let generation = core.generation;
    assert!(
        core.command(Command::RemoveTracks {
            track_ids: vec![1, 1, 999],
        })
        .is_err()
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.generation, generation);
    assert!(core.store.track(1).unwrap().is_some());
    core.command(Command::RemoveTracks {
        track_ids: vec![1, 1],
    })
    .unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.store.track(1).unwrap().is_none());
    assert!(
        core.store
            .queue_page(None, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .iter()
            .all(|entry| entry.track_id != 1)
    );
    assert_queue_saved(&core);
}

#[test]
fn failed_track_deletion_retains_persisted_current_occurrence_and_fences_audio() {
    for missing in [false, true] {
        for status in [
            PlaybackStatus::Stopped,
            PlaybackStatus::Paused,
            PlaybackStatus::Playing,
        ] {
            let (directory, mut core) = fixture_with_database(Some(10.0), true);
            if missing {
                let path = core.track(2).unwrap().path;
                let impacts = core
                    .store
                    .apply_scan(&ScanResult {
                        roots: vec![path],
                        ..ScanResult::default()
                    })
                    .unwrap();
                core.apply_scan_impacts(impacts).unwrap();
            }
            playing(&mut core, 3);
            match status {
                PlaybackStatus::Stopped => core.stop().unwrap(),
                PlaybackStatus::Paused => core.command(Command::Pause).unwrap(),
                PlaybackStatus::Playing => {}
            }
            core.save_queue().unwrap();
            let playlist = core
                .store
                .create_playlist_with_tracks("Keep", &[1, 2])
                .unwrap();
            core.playlist_created(playlist, true).unwrap();
            let connection =
                rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_track_delete BEFORE DELETE ON tracks
                 BEGIN SELECT RAISE(ABORT,'rejected track deletion'); END;",
                )
                .unwrap();
            let current = Arc::clone(core.state.current_track.as_ref().unwrap());
            let generation = core.generation;
            let late_started = started_event(&core, Some(10.0));
            let command = if missing {
                Command::RemoveMissingTracks
            } else {
                Command::RemoveTracks {
                    track_ids: vec![2, 2],
                }
            };
            let error = core.command(command).unwrap_err();
            assert!(format!("{error:#}").contains("rejected track deletion"));
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            assert_ne!(core.generation, generation);
            assert!(core.playback.is_none());
            assert_eq!(core.state.queue.current_id, Some(3));
            assert!(Arc::ptr_eq(
                core.state.current_track.as_ref().unwrap(),
                &current
            ));
            assert_eq!(
                queue_entries(&core),
                [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)]
            );
            assert_eq!(core.store.playlist_track_ids(playlist, 10).unwrap(), [1, 2]);
            assert_queue_saved(&core);
            core.audio_event(late_started).unwrap();
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
            assert_eq!(core.state.queue.current_id, Some(3));
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            core.load_initial_state().unwrap();
            assert_eq!(core.state.queue.current_id, Some(3));
            assert_eq!(core.state.current_track().unwrap().id, 2);
            connection
                .execute_batch("DROP TRIGGER reject_track_delete")
                .unwrap();
            let command = if missing {
                Command::RemoveMissingTracks
            } else {
                Command::RemoveTracks {
                    track_ids: vec![2, 2],
                }
            };
            core.command(command).unwrap();
            assert!(core.state.current_track.is_none());
            assert_eq!(core.state.queue.current_id, None);
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            assert_eq!(queue_entries(&core), [(1, 1), (4, 3), (5, 4)]);
            assert_queue_saved(&core);
        }
    }
}

#[test]
fn unrelated_track_deletion_preserves_current_occurrence_and_playback_status() {
    for missing in [false, true] {
        for status in [
            PlaybackStatus::Stopped,
            PlaybackStatus::Paused,
            PlaybackStatus::Playing,
        ] {
            let (_directory, mut core) = fixture();
            if missing {
                let path = core.track(1).unwrap().path;
                let impacts = core
                    .store
                    .apply_scan(&ScanResult {
                        roots: vec![path],
                        ..ScanResult::default()
                    })
                    .unwrap();
                core.apply_scan_impacts(impacts).unwrap();
            }
            playing(&mut core, 3);
            match status {
                PlaybackStatus::Stopped => core.stop().unwrap(),
                PlaybackStatus::Paused => core.command(Command::Pause).unwrap(),
                PlaybackStatus::Playing => {}
            }
            let generation = core.generation;
            let before = core.state.query_revisions();
            let scopes = Arc::clone(&core.state.library.playlist_entry_revisions);
            let command = if missing {
                Command::RemoveMissingTracks
            } else {
                Command::RemoveTracks { track_ids: vec![1] }
            };
            core.command(command).unwrap();
            assert_eq!(core.state.query_revisions().playlist, before.playlist);
            assert!(Arc::ptr_eq(
                &core.state.library.playlist_entry_revisions,
                &scopes
            ));
            assert_eq!(core.state.playback.status, status);
            assert_eq!(core.generation, generation);
            assert_eq!(core.state.queue.current_id, Some(3));
            assert_eq!(core.state.queue.current_position, Some(1));
            assert_eq!(core.state.current_track().unwrap().id, 2);
            assert_queue_saved(&core);
        }
    }
}

#[test]
fn removing_tracks_prunes_history_and_previous_retains_the_remaining_gap() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 4);
    core.state.playback.shuffle = true;
    core.played = vec![1, 2, 3, 4, 5];
    core.played_cursor = 4;
    core.command(Command::RemoveTracks { track_ids: vec![2] })
        .unwrap();
    assert_eq!(core.played, [1, 4, 5]);
    assert_eq!(core.played_cursor, 2);
    missing(&mut core, Command::Previous, 1);
    assert_eq!(core.played_cursor, 2);
    assert_queue_saved(&core);
}

#[test]
fn metadata_edit_refreshes_published_history_without_rewriting_old_snapshots() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    playing(&mut core, 2);
    core.publish();
    let before = core.shared.read().library.history.clone();
    let before_ids = before.iter().map(|row| row.track_id).collect::<Vec<_>>();
    let before_played_at = before
        .iter()
        .find(|row| row.track_id == 1)
        .unwrap()
        .played_at;
    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("Renamed history".to_owned()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    core.publish();
    let published = core.shared.read().library.history.clone();
    let row = published.iter().find(|row| row.track_id == 1).unwrap();
    assert_eq!(row.title, "Renamed history");
    assert_eq!(row.played_at, before_played_at);
    assert_eq!(
        published.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        before_ids
    );
    assert_eq!(
        published
            .iter()
            .find(|row| row.track_id == 2)
            .unwrap()
            .title,
        "Track 2"
    );
    assert_eq!(
        before.iter().find(|row| row.track_id == 1).unwrap().title,
        "Track 1"
    );
}

#[test]
fn current_metadata_edit_preflights_capacity_without_blocking_unrelated_facts() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let (commands, receive) = bounded(1);
    commands.send(AudioCommand::Volume(0.5)).unwrap();
    let _idle_commands = core.engine.replace_commands(commands);
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    let before = core.state.query_revisions();
    let edit = || Command::EditTrack {
        track_id: 1,
        title: Some("Current title".into()),
        artist: Some("Current artist".into()),
        album: Some("Current album".into()),
        decoder_hint: None,
    };

    assert!(core.command(edit()).is_err());
    assert_eq!(core.track(1).unwrap().title, "Track 1");
    assert_eq!(core.state.query_revisions(), before);
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));

    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: true,
    })
    .unwrap();
    let stats = core.store.mark_played(1, 7).unwrap();
    core.update_track_stats(1, stats).unwrap();
    core.command(Command::EditTrack {
        track_id: 2,
        title: Some("Unrelated title".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert_eq!(core.state.current_track().unwrap().title, "Track 1");
    assert!(core.state.current_track().unwrap().favorite);
    assert_eq!(core.state.current_track().unwrap().last_played, Some(7));
    assert!(matches!(receive.try_recv(), Ok(AudioCommand::Volume(_))));
    assert!(receive.is_empty());

    core.command(edit()).unwrap();
    let current = core.state.current_track().unwrap();
    assert_eq!(current.title, "Current title");
    assert_eq!(current.artist, "Current artist");
    assert_eq!(current.album, "Current album");
    assert!(matches!(
        receive.try_recv(),
        Ok(AudioCommand::Metadata { .. })
    ));
    assert!(
        receive.is_empty(),
        "metadata edits must not reopen playback"
    );
}

#[test]
fn committed_metadata_refresh_preserves_the_old_snapshot_until_audio_admission() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    let (commands, receive) = bounded(1);
    commands.send(AudioCommand::Volume(0.5)).unwrap();
    let _idle_commands = core.engine.replace_commands(commands);
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    core.store
        .edit_track(
            1,
            Some("Committed title"),
            Some("Artist"),
            Some("Album"),
            None,
        )
        .unwrap();

    let result = core.apply_scan_impacts(crate::store::ScanImpacts {
        changed: true,
        library: true,
        ..crate::store::ScanImpacts::default()
    });
    assert!(result.is_err());
    assert_eq!(core.track(1).unwrap().title, "Committed title");
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
    // Restoring the cached title still needs audio admission for omitted tags.
    assert!(
        core.command(Command::EditTrack {
            track_id: 1,
            title: Some(current.title.clone()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .is_err()
    );
    assert_eq!(core.track(1).unwrap().title, "Committed title");

    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("Committed title".into()),
        artist: Some("Artist".into()),
        album: Some("Album".into()),
        decoder_hint: None,
    })
    .unwrap();
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
    assert!(matches!(receive.try_recv(), Ok(AudioCommand::Volume(_))));

    core.refresh_queue_tracks().unwrap();
    let current = core.state.current_track().unwrap();
    assert_eq!(current.title, "Committed title");
    assert_eq!(current.artist, "Artist");
    assert_eq!(current.album, "Album");
    assert!(matches!(
        receive.try_recv(),
        Ok(AudioCommand::Metadata { .. })
    ));
    assert!(receive.is_empty());
}
