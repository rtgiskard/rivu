use super::*;

#[test]
fn metadata_history_refresh_uses_committed_facts_not_cached_entries_or_title_intent() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    core.store.mark_played(1, 123).unwrap();
    assert!(core.state.library.history.is_empty());
    core.edit_track(1, Some("Changed".into()), None, None, None, None)
        .unwrap();
    assert_eq!(core.state.library.history[0].title, "Changed");
    let history = Arc::clone(&core.state.library.history);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute("UPDATE tracks SET last_played='unreadable' WHERE id=3", [])
        .unwrap();
    // Title intent alone is not an effective history change, even when another
    // display value changes. A never-played title edit is likewise irrelevant.
    core.edit_track(
        1,
        Some("Changed".into()),
        Some("Artist".into()),
        None,
        None,
        None,
    )
    .unwrap();
    core.edit_track(2, Some("Never played".into()), None, None, None, None)
        .unwrap();
    assert!(!core.transport.history_pending);
    assert!(Arc::ptr_eq(&history, &core.state.library.history));
}

#[test]
fn never_played_deletions_do_not_dirty_history_but_preserve_pending_recovery() {
    for missing in [false, true] {
        for pending in [false, true] {
            let (directory, mut core) = fixture_with_database(Some(10.0));
            let history = Arc::clone(&core.state.library.history);
            let connection =
                rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
            connection
                .execute_batch(
                    "UPDATE tracks SET last_played='unreadable' WHERE id=3;
                 UPDATE tracks SET missing=1 WHERE id=2;",
                )
                .unwrap();
            core.transport.history_pending = pending;
            let result = if missing {
                core.remove_missing_tracks()
            } else {
                core.remove_tracks(vec![2])
            };
            assert_eq!(result.is_err(), pending);
            assert!(core.store.track(2).unwrap().is_none());
            assert_eq!(core.transport.history_pending, pending);
            assert!(Arc::ptr_eq(&history, &core.state.library.history));
            if pending {
                connection
                    .execute("UPDATE tracks SET last_played=123 WHERE id=3", [])
                    .unwrap();
                core.refresh_catalog_projections().unwrap();
                assert!(!core.transport.history_pending);
                assert_eq!(core.state.library.history[0].track_id, 3);
            }
        }
    }
}

#[test]
fn deletion_metadata_scope_overflow_keeps_every_playlist_entry_invalidation() {
    let bound = rivu_protocol::model::MAX_METADATA_SCOPES;
    for missing in [false, true] {
        for affected_count in [bound - 1, bound] {
            let (directory, mut core) = fixture_with_database(Some(10.0));
            core.command(Command::CreatePlaylistWithTracks {
                name: "Unrelated".into(),
                track_ids: vec![3],
            })
            .unwrap();
            for index in 0..affected_count {
                core.command(Command::CreatePlaylistWithTracks {
                    name: format!("Affected {index}"),
                    track_ids: vec![2, 4],
                })
                .unwrap();
            }
            if missing {
                let connection =
                    rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
                connection
                    .execute("UPDATE tracks SET missing=1 WHERE id=2", [])
                    .unwrap();
            }
            let track = Query::Track { track_id: 3 };
            let before_track = track.stamp(core.query_response(&track).revisions);
            let unrelated = Query::PlaylistEntries {
                playlist_id: 1,
                query: None,
                offset: 0,
                limit: 1,
            };
            let before_unrelated = unrelated.stamp(core.query_response(&unrelated).revisions);
            let entries = Arc::clone(&core.state.library.playlist_entry_revisions);
            if missing {
                core.remove_missing_tracks().unwrap();
            } else {
                core.remove_tracks(vec![2]).unwrap();
            }
            let exact = affected_count < bound;
            assert_eq!(
                track.stamp(core.query_response(&track).revisions) == before_track,
                exact
            );
            assert_eq!(
                unrelated.stamp(core.query_response(&unrelated).revisions) == before_unrelated,
                exact
            );
            assert_eq!(core.state.library.playlist_entry_revisions[&1], entries[&1]);
            for (&id, &revision) in entries.iter().filter(|(id, _)| **id != 1) {
                assert_ne!(core.state.library.playlist_entry_revisions[&id], revision);
                assert_eq!(core.store.playlist_track_ids(id, 10).unwrap(), [4]);
            }
        }
    }
}

#[test]
fn committed_history_removal_retains_session_suppression_when_refresh_fails() {
    let (directory, mut core) = fixture();
    pending(&mut core, 1);
    started(&mut core, Some(10.0)).unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute("UPDATE tracks SET last_played='unreadable' WHERE id=4", [])
        .unwrap();
    core.command(Command::RemoveHistory { track_ids: vec![1] })
        .unwrap_err();
    assert_eq!(core.track(1).unwrap().last_played, None);
    assert!(core.transport.session.as_ref().unwrap().history_removed);
    assert!(core.transport.history_pending);
    connection
        .execute("UPDATE tracks SET last_played=NULL WHERE id=4", [])
        .unwrap();
    core.command(Command::RemoveHistory { track_ids: vec![1] })
        .unwrap();
    assert!(!core.transport.history_pending);
    assert!(core.state.library.history.is_empty());
    progress(&mut core, 3.0, 3.0);
    core.stop().unwrap();
    assert_eq!(core.track(1).unwrap().last_played, None);
}

#[test]
fn history_removal_changes_only_selected_timestamps_and_preserves_projections() {
    let (_directory, mut core) = fixture();
    core.store.set_favorite(&[1], true).unwrap();
    core.store.increment_play_count(1).unwrap();
    for id in [1, 2, 4] {
        core.store.mark_played(id, id * 10).unwrap();
    }
    pending(&mut core, 1);
    core.replace_history(core.store.history(MAX_QUERY_ROWS).unwrap());
    let mut first = core.track(1).unwrap();
    let mut second = core.track(2).unwrap();
    let fourth = core.track(4).unwrap();
    let queue = core.store.queue_page(None, 0, 100).unwrap();
    let playlist = core
        .store
        .create_playlist_with_tracks("Keep entries", &[1, 2, 1])
        .unwrap();
    let entries = core
        .store
        .playlist_entries_page(playlist, None, 0, 100)
        .unwrap();
    let before = core.state.query_revisions();
    core.command(Command::RemoveHistory {
        track_ids: vec![2, 1, 1],
    })
    .unwrap();
    first.last_played = None;
    second.last_played = None;
    assert_eq!(core.track(1).unwrap(), first);
    assert_eq!(core.track(2).unwrap(), second);
    assert_eq!(core.track(4).unwrap(), fourth);
    assert_eq!(core.state.current_track().unwrap().last_played, None);
    assert_eq!(
        core.state
            .library
            .history
            .iter()
            .map(|row| row.track_id)
            .collect::<Vec<_>>(),
        [4]
    );
    assert_eq!(
        core.store
            .queue_page(None, 0, 100)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        queue.rows.iter().map(|row| row.id).collect::<Vec<_>>()
    );
    assert_eq!(
        core.store
            .playlist_entries_page(playlist, None, 0, 100)
            .unwrap(),
        entries
    );
    let after = core.state.query_revisions();
    assert_ne!(after.library, before.library);
    assert_eq!(after.queue, before.queue);
    assert_eq!(after.playlist, before.playlist);
    assert_eq!(after.structure, before.structure);
    core.command(Command::RemoveHistory {
        track_ids: vec![1, 2],
    })
    .unwrap();
    core.command(Command::RemoveHistory { track_ids: vec![] })
        .unwrap();
    assert_eq!(core.state.query_revisions(), after);
}

#[test]
fn history_removal_rolls_back_batch_and_does_not_suppress_on_failure() {
    let (directory, mut core) = fixture();
    for id in [1, 2] {
        core.store.mark_played(id, id * 10).unwrap();
    }
    pending(&mut core, 1);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_history_removal BEFORE UPDATE OF last_played ON tracks WHEN NEW.id=2 AND NEW.last_played IS NULL BEGIN SELECT RAISE(ABORT,'history rejected'); END;").unwrap();
    let before = core.state.query_revisions();
    assert!(
        core.command(Command::RemoveHistory {
            track_ids: vec![1, 2]
        })
        .is_err()
    );
    assert_eq!(core.track(1).unwrap().last_played, Some(10));
    assert_eq!(core.track(2).unwrap().last_played, Some(20));
    assert!(!core.transport.session.as_ref().unwrap().history_removed);
    assert!(
        core.command(Command::RemoveHistory {
            track_ids: vec![1, 999]
        })
        .is_err()
    );
    assert_eq!(core.track(1).unwrap().last_played, Some(10));
    assert_eq!(core.state.query_revisions(), before);
}

#[test]
fn removed_active_history_stays_removed_through_late_start_progress_and_stop() {
    for late_start in [false, true] {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        if !late_start {
            started(&mut core, Some(10.0)).unwrap();
        }
        let generation = core.transport.generation;
        core.command(Command::RemoveHistory { track_ids: vec![1] })
            .unwrap();
        assert_eq!(core.transport.generation, generation);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        if late_start {
            started(&mut core, Some(10.0)).unwrap();
        }
        progress(&mut core, 3.0, 3.0);
        assert_eq!(play_count(&core, 1), 1);
        core.transport.write_deferred = true;
        core.flush_deferred_playback().unwrap();
        assert_eq!(core.track(1).unwrap().last_played, None);
        core.stop().unwrap();
        assert_eq!(core.track(1).unwrap().last_played, None);
        assert!(core.store.history(MAX_QUERY_ROWS).unwrap().is_empty());
        pending(&mut core, 1);
        started(&mut core, Some(10.0)).unwrap();
        assert!(core.track(1).unwrap().last_played.is_some());
        assert_eq!(core.state.library.history[0].track_id, 1);
    }
}

#[test]
fn removed_retired_history_discards_deferred_activity_but_counts_heard_audio() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    started(&mut core, Some(10.0)).unwrap();
    let mut stats = core.transport.session.take().unwrap();
    stats.record_heard(3.0, 100);
    core.transport.retired = Some(super::super::playback::RetiredPlayback {
        stats,
        pending_close: None,
    });
    core.transport.write_deferred = true;
    core.command(Command::RemoveHistory { track_ids: vec![1] })
        .unwrap();
    core.flush_deferred_playback().unwrap();
    assert!(core.transport.retired.is_none());
    assert_eq!(play_count(&core, 1), 1);
    assert_eq!(core.track(1).unwrap().last_played, None);
    assert!(core.store.history(MAX_QUERY_ROWS).unwrap().is_empty());
}

#[test]
fn committed_track_edit_invalidates_before_failed_refresh_and_same_edit_is_noop() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
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
        release_date: None,
    };
    let before = core.state.query_revisions();
    core.command(edit()).unwrap_err();
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
    assert!(core.projections.current_track);
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
    core.recover_projections_on_demand();
    assert!(!core.projections.current_track);
    assert_eq!(core.state.current_track().unwrap().title, "Committed title");
    assert_eq!(core.state.query_revisions(), committed);
}

#[test]
fn committed_favorite_refresh_failure_recovers_without_replaying_toggle() {
    for toggle in [false, true] {
        let (directory, mut core) = fixture_with_database(Some(10.0));
        core.state.queue.current_id = Some(1);
        core.state.current_track = Some(Arc::new(core.track(1).unwrap()));
        core.save_queue().unwrap();
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_favorite_refresh AFTER UPDATE OF favorite ON tracks WHEN NEW.id=1 BEGIN UPDATE tracks SET channels='unreadable' WHERE id=1; END;").unwrap();
        let result = if toggle {
            core.toggle_favorite(&[1, 1])
        } else {
            core.set_favorite(&[1, 1], true)
        };
        assert!(result.is_err());
        assert!(core.projections.current_track);
        let committed = core.state.query_revisions();
        if !toggle {
            core.set_favorite(&[1], true).unwrap();
            assert_eq!(core.state.query_revisions(), committed);
        }
        connection
            .execute_batch(
                "DROP TRIGGER fail_favorite_refresh; UPDATE tracks SET channels=2 WHERE id=1;",
            )
            .unwrap();
        core.recover_projections_on_demand();
        assert!(!core.projections.current_track);
        assert!(core.state.current_track().unwrap().favorite);
        assert_eq!(core.state.query_revisions(), committed);
    }
}

#[test]
fn failed_edits_and_empty_deletions_do_not_invalidate() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
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
            release_date: None,
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("edit rejected"));
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
        let (directory, mut core) = fixture_with_database(Some(10.0));
        if missing {
            let path = core.track(2).unwrap().path;
            let impacts = core
                .store
                .apply_scan(&ScanResult {
                    roots: vec![path],
                    ..ScanResult::default()
                })
                .unwrap();
            core.apply_catalog_changes(impacts).unwrap();
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
        core.store.set_track_cache_query_batch_size(0);
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
        core.command(command).unwrap_err();
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
        assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
        assert!(Arc::ptr_eq(
            core.state.current_track.as_ref().unwrap(),
            &current
        ));
        core.command(Command::RemoveMissingTracks).unwrap();
        assert_eq!(core.state.query_revisions(), committed);
    }
}

#[test]
fn committed_deletions_isolate_history_and_queue_read_failures() {
    for missing in [false, true] {
        for fail_queue_projection in [false, true] {
            let (directory, mut core) = fixture_with_database(Some(10.0));
            if missing {
                let path = core.track(2).unwrap().path;
                let impacts = core
                    .store
                    .apply_scan(&ScanResult {
                        roots: vec![path],
                        ..ScanResult::default()
                    })
                    .unwrap();
                core.apply_catalog_changes(impacts).unwrap();
            }
            core.store.mark_played(2, 7).unwrap();
            core.state.library.history = Arc::new(core.store.history(MAX_QUERY_ROWS).unwrap());
            let history = Arc::clone(&core.state.library.history);
            assert_eq!(history[0].track_id, 2);
            core.store.set_queue_current(Some(1)).unwrap();
            core.refresh_queue_tracks().unwrap();
            let current = Arc::clone(core.state.current_track.as_ref().unwrap());
            let queue = core.state.queue.clone();
            core.transport.history = vec![1, 2, 3, 4, 5];
            core.transport.history_cursor = 4;
            let before = core.state.query_revisions();
            let connection =
                rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
            let failure = if fail_queue_projection {
                // The scalar projection requires the singleton metadata row.
                "DELETE FROM rivu_meta;"
            } else {
                // Only history reads this surviving, non-current row's timestamp.
                "UPDATE tracks SET last_played='unreadable' WHERE id=3;"
            };
            connection
                .execute_batch(&format!(
                    "CREATE TRIGGER fail_delete_refresh AFTER DELETE ON tracks WHEN OLD.id=2
                     BEGIN {failure} END;"
                ))
                .unwrap();
            let command = if missing {
                Command::RemoveMissingTracks
            } else {
                Command::RemoveTracks { track_ids: vec![2] }
            };
            core.command(command).unwrap_err();
            assert!(core.store.track(2).unwrap().is_none());
            assert_eq!(core.state.library.track_total, 3);
            let committed = core.state.query_revisions();
            assert_ne!(committed.library, before.library);
            assert_ne!(committed.structure, before.structure);
            if fail_queue_projection {
                assert!(core.state.library.history.is_empty());
                assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
                assert_eq!(core.state.queue.total, queue.total);
                assert_eq!(core.state.queue.current_id, queue.current_id);
                assert_eq!(core.state.queue.current_position, queue.current_position);
                assert_ne!(core.state.queue.revision, queue.revision);
                assert!(Arc::ptr_eq(
                    core.state.current_track.as_ref().unwrap(),
                    &current
                ));
                assert_eq!(core.transport.history, [1, 4, 5]);
                assert_eq!(core.transport.history_cursor, 2);
            } else {
                assert!(Arc::ptr_eq(&core.state.library.history, &history));
                assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
                assert_queue_saved(&core);
                assert_eq!(core.state.current_track().unwrap().id, 1);
                assert_eq!(core.transport.history, [1, 4, 5]);
                assert_eq!(core.transport.history_cursor, 2);
            }
        }
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
            decoder_hint: Some(rivu_format::decoder_hint::DecoderHint::Ffmpeg),
            release_date: None,
        };
        let artist = Command::EditTrack {
            track_id: 1,
            title: None,
            artist: Some("Artist".into()),
            album: None,
            decoder_hint: None,
            release_date: None,
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
        assert_eq!(
            current.decoder_hint,
            rivu_format::decoder_hint::DecoderHint::Ffmpeg
        );
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
            release_date: None,
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
        release_date: None,
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
            release_date: None,
        },
        Command::EditTrack {
            track_id: 1,
            title: Some("Track 1".into()),
            artist: None,
            album: None,
            decoder_hint: Some(DecoderHint::Auto),
            release_date: None,
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
            release_date: None,
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
    let generation = core.transport.generation;
    for hint in [DecoderHint::Ffmpeg, DecoderHint::Native, DecoderHint::Auto] {
        core.command(Command::EditTrack {
            track_id: 2,
            title: None,
            artist: None,
            album: None,
            decoder_hint: Some(hint),
            release_date: None,
        })
        .unwrap();
        core.publish();
        assert_eq!(core.state.current_track().unwrap().decoder_hint, hint);
        assert_eq!(core.track(2).unwrap().decoder_hint, hint);
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.state.queue.revision, revisions.queue);
        assert_eq!(core.state.playback, playback);
        assert_eq!(core.transport.generation, generation);
        assert!(core.transport.session.is_some());
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
            release_date: None,
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
fn date_edit_refreshes_current_without_audio_and_preserves_no_op_snapshots() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 1);
    core.publish();
    let (commands, receive) = async_channel::bounded(1);
    commands
        .try_send(audio::QueuedAudioCommand::test_unfenced(
            AudioCommand::Volume(0.5),
        ))
        .unwrap();
    let _idle_commands = core.engine.replace_commands(commands);
    let original = Arc::clone(core.state.current_track.as_ref().unwrap());
    let playback = core.state.playback.clone();
    let generation = core.transport.generation;
    let edit = |release_date| Command::EditTrack {
        track_id: 1,
        title: None,
        artist: None,
        album: None,
        decoder_hint: None,
        release_date,
    };

    for date in [Some("2026-11-13"), None] {
        let before = core.state.query_revisions();
        core.command(edit(Some(date.unwrap_or("").to_owned())))
            .unwrap();
        core.publish();
        assert_eq!(core.track(1).unwrap().release_date.as_deref(), date);
        assert_eq!(
            core.state.current_track().unwrap().release_date.as_deref(),
            date
        );
        assert_ne!(core.state.query_revisions().library, before.library);
        assert_eq!(core.state.query_revisions().queue, before.queue);
        assert_eq!(core.state.playback, playback);
        assert_eq!(core.transport.generation, generation);

        let current = Arc::clone(core.state.current_track.as_ref().unwrap());
        let revisions = core.state.query_revisions();
        let sections = core.state.revisions;
        for patch in [None, Some(date.unwrap_or("").to_owned())] {
            core.command(edit(patch)).unwrap();
            core.publish();
            assert_eq!(core.state.query_revisions(), revisions);
            assert_eq!(core.state.revisions, sections);
            assert!(Arc::ptr_eq(
                core.state.current_track.as_ref().unwrap(),
                &current
            ));
        }
    }
    assert!(original.release_date.is_none());
    assert!(matches!(
        receive.try_recv().map(|queued| queued.command),
        Ok(AudioCommand::Volume(_))
    ));
    assert!(
        receive.is_empty(),
        "date edits must not send audio commands"
    );
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
            release_date: None,
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
        decoder_hint: Some(rivu_format::decoder_hint::DecoderHint::Native),
        release_date: None,
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
fn cue_rescan_preserves_loaded_source_and_seek_range_until_explicit_restart() {
    for status in [PlaybackStatus::Playing, PlaybackStatus::Paused] {
        for end_frame in [225, 750] {
            let (directory, mut core) = fixture();
            let source = directory.path().join("1.wav");
            let replacement = directory.path().join("2.wav");
            std::fs::write(&source, []).unwrap();
            std::fs::write(&replacement, []).unwrap();
            let known = core
                .store
                .stored_scan_files(std::slice::from_ref(&replacement))
                .unwrap()
                .pop()
                .unwrap();
            let mut media = known.media.unwrap();
            media.codec = "replacement-codec".into();
            media.sample_rate = 96_000;
            media.channels = 1;
            media.bits_per_sample = Some(24);
            media.bitrate_bps = Some(2_304_000);
            media.bitrate_exact = true;
            let impact = core
                .store
                .apply_scan(&ScanResult {
                    records: vec![Arc::new(ScanRecord {
                        path: known.path,
                        size: known.size,
                        modified_ns: known.modified_ns,
                        fingerprint: known.fingerprint,
                        media,
                    })],
                    ..ScanResult::default()
                })
                .unwrap();
            core.apply_catalog_changes(impact).unwrap();
            let original = CueSegment {
                sheet: directory.path().join("album.cue"),
                number: 1,
                start_frame: 75,
                end_frame: Some(450),
            };
            let track_id = register_cue(&mut core, &source, original.clone(), "Original segment");
            core.enqueue(&[track_id]).unwrap();
            let queue_id = core
                .store
                .queue_id_at(core.state.queue.total - 1)
                .unwrap()
                .unwrap();
            let commands = recording_audio(&mut core);
            core.start_with_status(queue_id, QueueHistory::Preserve, status)
                .unwrap();
            assert!(matches!(
                commands.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioCommand::StopAndSnapshot(_)
            ));
            assert!(matches!(
                commands.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioCommand::Load { .. }
            ));
            started(&mut core, Some(5.0)).unwrap();
            core.publish();
            let before = core.shared.read().clone();
            let generation = core.transport.generation;
            let changed = CueSegment {
                start_frame: 150,
                end_frame: Some(end_frame),
                ..original.clone()
            };
            assert_eq!(
                register_cue(
                    &mut core,
                    &replacement,
                    changed.clone(),
                    "Rescanned segment"
                ),
                track_id
            );
            core.publish();
            let published = core.shared.read().clone();
            let current = published.current_track().unwrap();
            assert_eq!(current.path, source);
            assert_eq!(current.cue.as_ref(), Some(&original));
            assert_eq!(current.duration, Some(5.0));
            assert_eq!(current.title, "Rescanned segment");
            let original_metadata = before.current_track().unwrap();
            assert_eq!(current.codec, original_metadata.codec);
            assert_eq!(current.sample_rate, original_metadata.sample_rate);
            assert_eq!(current.channels, original_metadata.channels);
            assert_eq!(current.bits_per_sample, original_metadata.bits_per_sample);
            assert_eq!(current.bitrate_bps, original_metadata.bitrate_bps);
            assert_eq!(current.bitrate_exact, original_metadata.bitrate_exact);
            assert_eq!(published.playback, before.playback);
            assert_eq!(published.queue.current_id, Some(queue_id));
            assert_eq!(core.transport.generation, generation);
            let stored = core.track(track_id).unwrap();
            assert_eq!(stored.path, replacement);
            assert_eq!(stored.cue.as_ref(), Some(&changed));
            assert_eq!(stored.duration, Some((end_frame - 150) as f64 / 75.0));
            assert_eq!(stored.codec, "replacement-codec");
            assert_eq!(stored.sample_rate, 96_000);
            assert_eq!(stored.channels, 1);
            assert!(matches!(
                commands.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioCommand::Metadata { generation: value, metadata }
                    if value == generation && metadata.title == "Rescanned segment"
            ));
            core.seek(99.0).unwrap();
            assert_eq!(core.state.playback.position, 5.0);
            assert_eq!(core.state.playback.status, status);
            assert!(matches!(
                commands.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioCommand::Seek { seconds: 5.0, .. }
            ));
            assert!(commands.is_empty());

            core.start_with_status(queue_id, QueueHistory::Preserve, status)
                .unwrap();
            assert!(matches!(
                commands.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioCommand::StopAndSnapshot(_)
            ));
            match commands.recv_timeout(Duration::from_secs(2)).unwrap() {
                AudioCommand::Load {
                    path,
                    range: Some(range),
                    paused,
                    ..
                } => {
                    assert_eq!(path, replacement);
                    assert_eq!(range.start_seconds, 2.0);
                    assert_eq!(range.end_seconds, Some(end_frame as f64 / 75.0));
                    assert_eq!(paused, status == PlaybackStatus::Paused);
                }
                _ => panic!("explicit restart must load the rescanned CUE source"),
            }
            assert_eq!(
                core.state.current_track().unwrap().cue.as_ref(),
                Some(&changed)
            );
            assert_eq!(before.current_track().unwrap().title, "Original segment");
        }
    }
}

#[test]
fn favorite_edits_reuse_unaffected_history_and_keep_published_snapshots_immutable() {
    let (_directory, mut core) = fixture();
    core.store.mark_played(1, 7).unwrap();
    core.store.mark_played(4, 8).unwrap();
    core.replace_history(core.store.history(MAX_QUERY_ROWS).unwrap());
    core.publish();
    let before = core.shared.read().library.history.clone();
    for command in [
        Command::SetFavorite {
            track_ids: vec![2, 2, 3],
            favorite: true,
        },
        Command::ToggleFavorite {
            track_ids: vec![2, 2, 3],
        },
        Command::SetFavorite {
            track_ids: vec![2],
            favorite: true,
        },
        // The selected history row already has the requested value, while the
        // other selected row changes in the catalog.
        Command::SetFavorite {
            track_ids: vec![1, 2],
            favorite: false,
        },
    ] {
        core.command(command).unwrap();
        core.publish();
        assert!(Arc::ptr_eq(&core.shared.read().library.history, &before));
    }
    core.command(Command::ToggleFavorite {
        track_ids: vec![1, 4, 1, 4],
    })
    .unwrap();
    core.publish();
    let after = core.shared.read().library.history.clone();
    assert!(after.iter().all(|entry| entry.favorite));
    assert!(before.iter().all(|entry| !entry.favorite));
    let history_facts = |history: &[rivu_protocol::model::HistoryEntry]| {
        history
            .iter()
            .map(|entry| (entry.track_id, entry.played_at, entry.play_count))
            .collect::<Vec<_>>()
    };
    assert_eq!(history_facts(&after), history_facts(&before));
    assert!(core.track(1).unwrap().favorite);
    assert!(core.track(4).unwrap().favorite);
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
    core.apply_catalog_changes(impacts).unwrap();
    playing(&mut core, 2);
    core.transport.history = vec![1, 2, 3];
    core.transport.history_cursor = 2;
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
    assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
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
    assert_eq!(core.transport.history, [1]);
    assert_eq!(core.transport.history_cursor, 1);
    assert_eq!(core.state.queue.current_id, None);
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.state.library.history.is_empty());
    assert_eq!(std::fs::read(&kept_path).unwrap(), b"Do not delete");
}

#[test]
fn removing_missing_tracks_retires_session_after_failed_scan_projection() {
    for selected_track in [Some(1), Some(2), None] {
        let (directory, mut core) = fixture();
        playing(&mut core, 1);
        let generation = core.transport.generation;
        let late_started = started_event(&core, Some(10.0));
        let current = Arc::clone(core.state.current_track.as_ref().unwrap());
        core.store
            .apply_scan(&ScanResult {
                roots: vec![current.path.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        assert!(core.store.track(1).unwrap().unwrap().missing);
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE saved_queue_meta AS SELECT * FROM rivu_meta;
                 DELETE FROM rivu_meta;",
            )
            .unwrap();
        assert!(core.refresh_queue_tracks().is_err());
        assert!(core.transport.projection_stale);
        assert!(Arc::ptr_eq(
            core.state.current_track.as_ref().unwrap(),
            &current
        ));
        assert!(!core.state.current_track().unwrap().missing);
        connection
            .execute_batch(
                "INSERT INTO rivu_meta SELECT * FROM saved_queue_meta;
                 DROP TABLE saved_queue_meta;",
            )
            .unwrap();

        // A missing or unrelated UI selection must not hide the live source.
        core.state.current_track = selected_track.map(|id| {
            if id == 1 {
                Arc::clone(&current)
            } else {
                Arc::new(core.track(id).unwrap())
            }
        });
        core.command(Command::RemoveMissingTracks).unwrap();
        assert!(core.store.track(1).unwrap().is_none());
        assert!(core.transport.session.is_none());
        assert!(core.transport.retired.is_none());
        assert_ne!(core.transport.generation, generation);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert!(core.state.current_track.is_none());
        assert_eq!(core.state.queue.current_id, None);
        assert_eq!(queue_entries(&core), [(2, 2), (3, 2), (4, 3), (5, 4)]);
        assert_queue_saved(&core);

        core.audio_event(late_started).unwrap();
        core.audio_event(AudioEvent::Ended {
            generation,
            completed_duration: None,
            bitrate_bps: None,
        })
        .unwrap();
        assert!(core.transport.session.is_none());
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.queue.current_id, None);
    }
}

#[test]
fn removing_missing_tracks_preserves_session_when_only_cached_track_is_missing() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let generation = core.transport.generation;
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            roots: vec![core.track(2).unwrap().path],
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_catalog_changes(impacts).unwrap();
    Arc::make_mut(core.state.current_track.as_mut().unwrap()).missing = true;
    assert!(!core.store.track(1).unwrap().unwrap().missing);

    core.command(Command::RemoveMissingTracks).unwrap();
    assert!(core.store.track(2).unwrap().is_none());
    assert_eq!(core.transport.session.as_ref().unwrap().track_id, 1);
    assert_eq!(core.transport.generation, generation);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.state.current_track().unwrap().id, 1);
    assert!(!core.state.current_track().unwrap().missing);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(queue_entries(&core), [(1, 1), (4, 3), (5, 4)]);
    assert_queue_saved(&core);
}

#[test]
fn track_removal_validates_the_set_before_stopping_playback() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let generation = core.transport.generation;
    assert!(
        core.command(Command::RemoveTracks {
            track_ids: vec![1, 1, 999],
        })
        .is_err()
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.transport.generation, generation);
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
            let (directory, mut core) = fixture_with_database(Some(10.0));
            if missing {
                let path = core.track(2).unwrap().path;
                let impacts = core
                    .store
                    .apply_scan(&ScanResult {
                        roots: vec![path],
                        ..ScanResult::default()
                    })
                    .unwrap();
                core.apply_catalog_changes(impacts).unwrap();
            }
            playing(&mut core, 3);
            let generation = core.transport.generation;
            let late_started = started_event(&core, Some(10.0));
            match status {
                PlaybackStatus::Stopped => core.stop().unwrap(),
                PlaybackStatus::Paused => {
                    core.command(Command::Pause).unwrap();
                }
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
            assert!(core.transport.session.is_none());
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
                completed_duration: None,
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
                core.apply_catalog_changes(impacts).unwrap();
            }
            playing(&mut core, 3);
            match status {
                PlaybackStatus::Stopped => core.stop().unwrap(),
                PlaybackStatus::Paused => {
                    core.command(Command::Pause).unwrap();
                }
                PlaybackStatus::Playing => {}
            }
            let generation = core.transport.generation;
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
            assert_eq!(core.transport.generation, generation);
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
    edit_preferences(&mut core, |config| config.shuffle = true);
    core.transport.history = vec![1, 2, 3, 4, 5];
    core.transport.history_cursor = 4;
    core.command(Command::RemoveTracks { track_ids: vec![2] })
        .unwrap();
    assert_eq!(core.transport.history, [1, 4, 5]);
    assert_eq!(core.transport.history_cursor, 2);
    missing(&mut core, Command::Previous, 1);
    assert_eq!(core.transport.history_cursor, 2);
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
        release_date: None,
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
    let (commands, receive) = async_channel::bounded(1);
    commands
        .try_send(audio::QueuedAudioCommand::test_unfenced(
            AudioCommand::Volume(0.5),
        ))
        .unwrap();
    let _idle_commands = core.engine.replace_commands(commands);
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    let before = core.state.query_revisions();
    let edit = || Command::EditTrack {
        track_id: 1,
        title: Some("Current title".into()),
        artist: Some("Current artist".into()),
        album: Some("Current album".into()),
        decoder_hint: None,
        release_date: None,
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
        release_date: None,
    })
    .unwrap();
    assert_eq!(core.state.current_track().unwrap().title, "Track 1");
    assert!(core.state.current_track().unwrap().favorite);
    assert_eq!(core.state.current_track().unwrap().last_played, Some(7));
    assert!(matches!(
        receive.try_recv().map(|queued| queued.command),
        Ok(AudioCommand::Volume(_))
    ));
    assert!(receive.is_empty());

    core.command(edit()).unwrap();
    let current = core.state.current_track().unwrap();
    assert_eq!(current.title, "Current title");
    assert_eq!(current.artist, "Current artist");
    assert_eq!(current.album, "Current album");
    assert!(matches!(
        receive.try_recv().map(|queued| queued.command),
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
    let (commands, receive) = async_channel::bounded(1);
    commands
        .try_send(audio::QueuedAudioCommand::test_unfenced(
            AudioCommand::Volume(0.5),
        ))
        .unwrap();
    let _idle_commands = core.engine.replace_commands(commands);
    let current = Arc::clone(core.state.current_track.as_ref().unwrap());
    core.store
        .edit_track(
            1,
            Some("Committed title"),
            Some("Artist"),
            Some("Album"),
            None,
            None,
        )
        .unwrap();

    let result = core.apply_catalog_changes(crate::store::CatalogChanges {
        changed: true,
        library: true,
        ..crate::store::CatalogChanges::default()
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
            release_date: None,
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
        release_date: None,
    })
    .unwrap();
    assert!(Arc::ptr_eq(
        core.state.current_track.as_ref().unwrap(),
        &current
    ));
    assert!(matches!(
        receive.try_recv().map(|queued| queued.command),
        Ok(AudioCommand::Volume(_))
    ));

    core.refresh_queue_tracks().unwrap();
    let current = core.state.current_track().unwrap();
    assert_eq!(current.title, "Committed title");
    assert_eq!(current.artist, "Artist");
    assert_eq!(current.album, "Album");
    assert!(matches!(
        receive.try_recv().map(|queued| queued.command),
        Ok(AudioCommand::Metadata { .. })
    ));
    assert!(receive.is_empty());
}

#[test]
fn committed_deletion_invalidates_all_affected_playlists_before_projection_failure() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    for (name, track_ids) in [("First", vec![2, 3]), ("Second", vec![2, 4])] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids,
        })
        .unwrap();
    }
    core.store.mark_played(2, 123).unwrap();
    let before = core.state.query_revisions();
    let second = core.state.library.playlist_entry_revisions[&2];
    let first = core.state.library.playlist_entry_revisions[&1];
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_delete_projection AFTER DELETE ON tracks WHEN OLD.id=2
             BEGIN UPDATE tracks SET last_played='unreadable' WHERE id=3; END;",
        )
        .unwrap();
    core.command(Command::RemoveTracks { track_ids: vec![2] })
        .unwrap_err();
    assert_eq!(core.state.library.track_total, 3);
    assert_eq!(core.state.library.playlist_revision, before.playlist + 1);
    assert_eq!(
        core.state.library.duplicate_members_revision,
        before.duplicate_members + 1
    );
    assert_ne!(core.state.library.playlist_entry_revisions[&2], second);
    assert_ne!(core.state.library.playlist_entry_revisions[&1], first);
    assert_eq!(core.store.queue_ids().unwrap(), [1, 4, 5]);
    assert_eq!(core.state.queue.total, 3);
    assert_eq!(core.store.playlist_track_ids(1, 20).unwrap(), [3]);
    assert_eq!(core.store.playlist_track_ids(2, 20).unwrap(), [4]);
}

#[test]
fn album_queries_keep_cached_members_for_unrelated_edits_and_invalidate_member_changes() {
    let (_directory, mut core) = fixture();
    core.command(Command::EditTrack {
        track_id: 1,
        title: None,
        artist: None,
        album: Some("Scoped album".into()),
        decoder_hint: None,
        release_date: None,
    })
    .unwrap();
    let query = Query::AlbumTracks {
        key: AlbumKey {
            directory: core.track(1).unwrap().path.parent().unwrap().into(),
            title: "Scoped album".into(),
        },
        query: None,
        offset: 0,
        limit: 10,
    };
    let before = query.stamp(core.query_response(&query).revisions);
    core.command(Command::EditTrack {
        track_id: 2,
        title: Some("Unrelated edit".into()),
        artist: None,
        album: None,
        decoder_hint: None,
        release_date: None,
    })
    .unwrap();
    assert_eq!(query.stamp(core.query_response(&query).revisions), before);
    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("Changed member".into()),
        artist: None,
        album: None,
        decoder_hint: None,
        release_date: None,
    })
    .unwrap();
    let response = core.query_response(&query);
    assert_ne!(query.stamp(response.revisions), before);
    let ViewResponse::AlbumTracks(page) = response.result.unwrap() else {
        panic!("expected album members")
    };
    assert_eq!(page.rows[0].title, "Changed member");
}
