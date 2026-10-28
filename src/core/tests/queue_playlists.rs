use super::*;

#[test]
fn indexed_playlist_sources_and_after_moves_update_only_the_target_projection() {
    let (_directory, mut core) = fixture();
    for name in ["target", "other"] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids: vec![2],
        })
        .unwrap();
    }
    let other = core.state.playlist_query_revisions(2).playlist_entries;
    let baseline = core.state.playlist_query_revisions(1);
    core.command(Command::InsertPlaylistSources {
        other_tracks: false,
        playlist_id: 1,
        directories: vec![],
        track_ids: vec![4, 1, 4, 3],
        index: 1,
        expected_revision: baseline.playlist_entries.unwrap().revision,
    })
    .unwrap();
    let inserted = core.state.playlist_query_revisions(1);
    assert_ne!(inserted.playlist_entries, baseline.playlist_entries);
    assert_ne!(inserted.playlist, baseline.playlist);
    assert_ne!(inserted.duplicate_members, baseline.duplicate_members);
    assert_eq!(
        core.state.playlist_query_revisions(2).playlist_entries,
        other
    );
    let rows = core
        .store
        .playlist_entries_page(1, None, 0, 20)
        .unwrap()
        .rows;
    assert_eq!(
        rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        [2, 4, 1, 4, 3]
    );
    core.command(Command::MovePlaylistEntries {
        playlist_id: 1,
        entry_ids: vec![rows[0].id],
        before_entry_id: Some(rows[2].id),
        after: true,
        expected_revision: inserted.playlist_entries.unwrap().revision,
    })
    .unwrap();
    let moved = core.state.playlist_query_revisions(1);
    assert_ne!(moved.playlist_entries, inserted.playlist_entries);
    assert_eq!(moved.playlist, inserted.playlist);
    assert_eq!(moved.duplicate_members, inserted.duplicate_members);
    assert_eq!(
        core.state.playlist_query_revisions(2).playlist_entries,
        other
    );
    assert_eq!(
        core.store
            .playlist_entries_page(1, None, 0, 20)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.track_id)
            .collect::<Vec<_>>(),
        [4, 1, 2, 4, 3]
    );
    assert!(
        core.command(Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![],
            before_entry_id: None,
            after: true,
            expected_revision: inserted.playlist_entries.unwrap().revision,
        })
        .is_err()
    );
    core.command(Command::MovePlaylistEntries {
        playlist_id: 1,
        entry_ids: vec![rows[0].id],
        before_entry_id: Some(rows[0].id),
        after: true,
        expected_revision: moved.playlist_entries.unwrap().revision,
    })
    .unwrap();
    assert_eq!(core.state.playlist_query_revisions(1), moved);
}

#[test]
fn indexed_queue_commands_preserve_playing_occurrence_and_reject_stale_empty_intents() {
    let (_directory, mut core) = fixture();
    pending(&mut core, 3);
    core.record_queue_history(3);
    let audio = recording_audio(&mut core);
    let revision = core.state.queue.revision;
    core.command(Command::InsertQueue {
        track_ids: vec![4, 1, 4],
        index: 1,
        expected_revision: revision,
    })
    .unwrap();
    assert_eq!(
        queue_entries(&core)
            .iter()
            .map(|&(_, track)| track)
            .collect::<Vec<_>>(),
        [1, 4, 1, 4, 2, 2, 3, 4]
    );
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(5));
    assert_eq!(core.state.current_track().unwrap().id, 2);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.played, [3]);
    assert_eq!(core.state.queue.revision, revision + 1);
    let state = core.state.queue.clone();
    let ids = core.queue_ids.clone();
    for command in [
        Command::InsertQueue {
            track_ids: vec![],
            index: 0,
            expected_revision: revision,
        },
        Command::InsertQueueSources {
            other_tracks: false,
            directories: vec![],
            track_ids: vec![],
            index: 0,
            expected_revision: revision,
        },
        Command::InsertQueue {
            track_ids: vec![],
            index: state.total + 1,
            expected_revision: state.revision,
        },
        Command::InsertQueueSources {
            other_tracks: false,
            directories: vec![],
            track_ids: vec![],
            index: state.total + 1,
            expected_revision: state.revision,
        },
        Command::InsertQueue {
            track_ids: vec![1, 999],
            index: 0,
            expected_revision: state.revision,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.queue_ids, ids);
        assert_eq!(core.state.queue, state);
    }
    core.command(Command::InsertQueueSources {
        other_tracks: false,
        directories: vec![],
        track_ids: vec![4, 1, 4],
        index: state.total,
        expected_revision: state.revision,
    })
    .unwrap();
    assert_eq!(
        queue_entries(&core)[state.total..]
            .iter()
            .map(|&(_, track)| track)
            .collect::<Vec<_>>(),
        [4, 1, 4]
    );
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert!(audio.try_iter().next().is_none());
}

#[test]
fn indexed_playlist_commands_advance_only_changed_members_and_reject_stale_drops() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "insert".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let baseline = core.state.playlist_query_revisions(1);
    let token = baseline.playlist_entries.unwrap().revision;
    let queue = core.queue_ids.clone();
    core.command(Command::InsertPlaylist {
        playlist_id: 1,
        track_ids: vec![4, 2, 3, 4],
        index: 1,
        expected_revision: token,
    })
    .unwrap();
    let after = core.state.playlist_query_revisions(1);
    let next = after.playlist_entries.unwrap().revision;
    assert_ne!(next, token);
    assert_ne!(after.playlist, baseline.playlist);
    assert_ne!(after.duplicate_members, baseline.duplicate_members);
    assert_eq!(after.library, baseline.library);
    assert_eq!(after.structure, baseline.structure);
    let rows = core
        .store
        .playlist_entries_page(1, None, 0, 20)
        .unwrap()
        .rows;
    assert_eq!(
        rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        [1, 4, 2, 3, 4, 2]
    );
    for command in [
        Command::InsertPlaylist {
            playlist_id: 1,
            track_ids: vec![],
            index: 0,
            expected_revision: token,
        },
        Command::InsertPlaylistSources {
            other_tracks: false,
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![1],
            index: 0,
            expected_revision: token,
        },
        Command::InsertPlaylist {
            playlist_id: 1,
            track_ids: vec![1],
            index: rows.len() + 1,
            expected_revision: next,
        },
        Command::InsertPlaylistSources {
            other_tracks: false,
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![],
            index: rows.len() + 1,
            expected_revision: next,
        },
        Command::InsertPlaylist {
            playlist_id: 1,
            track_ids: vec![1, 999],
            index: 0,
            expected_revision: next,
        },
        Command::InsertPlaylist {
            playlist_id: 999,
            track_ids: vec![],
            index: 0,
            expected_revision: next,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.playlist_query_revisions(1), after);
    }
    core.command(Command::InsertPlaylist {
        playlist_id: 1,
        track_ids: vec![],
        index: 0,
        expected_revision: next,
    })
    .unwrap();
    core.command(Command::InsertPlaylistSources {
        other_tracks: false,
        playlist_id: 1,
        directories: vec![],
        track_ids: vec![],
        index: rows.len(),
        expected_revision: next,
    })
    .unwrap();
    assert_eq!(core.state.playlist_query_revisions(1), after);
    assert_eq!(core.queue_ids, queue);
}

#[test]
fn playlist_creation_count_failure_keeps_the_committed_lifetime_registered() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    // Fixture a committed producer result before exercising Core's completion.
    let playlist_id = core.store.create_playlist("Committed playlist").unwrap();
    let before = core.state.query_revisions();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE playlists;")
        .unwrap();
    assert!(core.playlist_created(playlist_id, false).is_err());
    assert_ne!(core.state.library.playlist_revision, before.playlist);
    assert!(
        core.state
            .library
            .playlist_entry_revisions
            .contains_key(&playlist_id)
    );
    assert_eq!(core.state.library.playlist_entry_revisions.len(), 1);
    assert_eq!(core.state.library.playlist_total, 0);
    assert_eq!(core.state.library.revision, before.library);
    assert_eq!(
        core.state.library.duplicate_members_revision,
        before.duplicate_members
    );
    assert_eq!(core.state.queue.revision, before.queue);
}

#[test]
fn sql_queue_restores_order_duplicates_current_and_clear_does_not_reuse_ids() {
    let (_directory, mut core) = fixture();
    core.state.queue.current_id = Some(3);
    core.save_queue().unwrap();
    core.queue_ids.clear();
    core.state.queue = QueueState::default();
    core.load_initial_state().unwrap();
    assert_eq!(
        queue_entries(&core),
        [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)]
    );
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(2));
    assert_eq!(core.state.current_track().unwrap().id, 2);
    core.command(Command::ClearQueue).unwrap();
    core.enqueue(&[1]).unwrap();
    assert!(core.queue_ids[0] > 5);
    core.load_initial_state().unwrap();
    assert_eq!(core.state.queue.total, 1);
    assert!(core.state.current_track.is_none());
}

#[test]
fn playlist_mutations_preserve_track_traversal_and_playback_sections() {
    let (_directory, mut core) = fixture();
    core.publish();
    let track_revision = core.state.query_revisions();
    let sections = core.state.revisions;
    let operations = [
        (
            Command::CreatePlaylist {
                name: "empty".into(),
            },
            1,
            false,
        ),
        (
            Command::CreatePlaylistWithTracks {
                name: "mix".into(),
                track_ids: vec![1, 2],
            },
            2,
            true,
        ),
        (
            Command::RenamePlaylist {
                playlist_id: 2,
                name: "renamed".into(),
            },
            2,
            false,
        ),
        (
            Command::AddPlaylist {
                playlist_id: 2,
                track_ids: vec![3],
            },
            2,
            true,
        ),
        (
            Command::AddPlaylistSources {
                other_tracks: false,
                playlist_id: 2,
                directories: vec![],
                track_ids: vec![4],
            },
            2,
            true,
        ),
        (Command::RemovePlaylistEntry { entry_id: 1 }, 2, true),
        (Command::DeletePlaylist { playlist_id: 1 }, 1, false),
        (Command::DeletePlaylist { playlist_id: 2 }, 0, true),
    ];
    for (command, total, members_changed) in operations {
        let command = match command {
            Command::RemovePlaylistEntry { .. } => Command::RemovePlaylistEntry {
                entry_id: core
                    .store
                    .playlist_entries_page(2, None, 0, 1)
                    .unwrap()
                    .rows[0]
                    .id,
            },
            command => command,
        };
        let before = core.state.query_revisions();
        core.command(command).unwrap();
        core.publish();
        assert_eq!(core.state.library.playlist_total, total);
        let after = core.state.query_revisions();
        assert_eq!(after.library, track_revision.library);
        assert_eq!(after.duplicate_groups, before.duplicate_groups);
        assert_eq!(
            after.duplicate_members != before.duplicate_members,
            members_changed
        );
        assert_eq!(
            core.state.query_revisions().structure,
            track_revision.structure
        );
        assert_eq!(core.state.revisions.queue, sections.queue);
        assert_eq!(core.state.revisions.playback, sections.playback);
        let ViewResponse::TrackBatch(batch) = core
            .view(&Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: Some(1),
                limit: 2,
                expected: Some(track_revision),
            })
            .unwrap()
        else {
            panic!("expected track batch")
        };
        assert_eq!(
            batch.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(
            core.view(&Query::PlaylistSummaryBatch {
                query: None,
                after: None,
                limit: 2,
                expected: Some(before),
            })
            .is_err()
        );
    }
}

#[test]
fn playlist_group_commands_reject_stale_intents_and_preserve_noop_revisions() {
    let (_directory, mut core) = fixture();
    for name in ["one", "two"] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids: vec![1, 2, 3, 4],
        })
        .unwrap();
    }
    let entries = core
        .store
        .playlist_entries_page(1, None, 0, 4)
        .unwrap()
        .rows;
    let ids: Vec<_> = entries.iter().map(|row| row.id).collect();
    let baseline = core.state.playlist_query_revisions(1);
    let token = baseline.playlist_entries.unwrap().revision;
    for command in [
        Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![],
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0], ids[1]],
            before_entry_id: Some(ids[2]),
            expected_revision: token,
            after: false,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0], ids[2]],
            before_entry_id: Some(ids[2]),
            expected_revision: token,
            after: false,
        },
    ] {
        core.command(command).unwrap();
        assert_eq!(core.state.playlist_query_revisions(1), baseline);
    }
    for command in [
        Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0], i64::MAX],
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0]],
            before_entry_id: Some(i64::MAX),
            expected_revision: token,
            after: false,
        },
        Command::MovePlaylistEntries {
            playlist_id: 999,
            entry_ids: vec![],
            before_entry_id: None,
            expected_revision: token,
            after: false,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.playlist_query_revisions(1), baseline);
        assert_eq!(
            core.store
                .playlist_entries_page(1, None, 0, 4)
                .unwrap()
                .rows,
            entries
        );
    }
    let other = core
        .store
        .playlist_entries_page(2, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::MovePlaylistEntry {
        entry_id: other,
        index: 3,
    })
    .unwrap();
    assert_eq!(core.state.playlist_query_revisions(1), baseline);
    core.command(Command::MovePlaylistEntries {
        playlist_id: 1,
        entry_ids: vec![ids[2], ids[0]],
        before_entry_id: None,
        expected_revision: token,
        after: false,
    })
    .unwrap();
    let reordered = core.state.playlist_query_revisions(1);
    assert_eq!(core.state.library.playlist_total, 2);
    assert_eq!(reordered.duplicate_members, baseline.duplicate_members);
    assert_eq!(reordered.playlist, baseline.playlist);
    assert_ne!(reordered.playlist_entries, baseline.playlist_entries);
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4, 1, 3]);
    for command in [
        Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[0]],
            expected_revision: token,
        },
        Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![],
            before_entry_id: None,
            expected_revision: token,
            after: false,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.playlist_query_revisions(1), reordered);
        assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4, 1, 3]);
    }
    core.command(Command::RemovePlaylistEntries {
        playlist_id: 1,
        entry_ids: vec![ids[2], ids[0]],
        expected_revision: reordered.playlist_entries.unwrap().revision,
    })
    .unwrap();
    assert_eq!(core.state.library.playlist_total, 2);
    let removed = core.state.playlist_query_revisions(1);
    assert_ne!(removed.duplicate_members, reordered.duplicate_members);
    assert_ne!(removed.playlist, reordered.playlist);
    assert_ne!(removed.playlist_entries, reordered.playlist_entries);
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4]);
    assert!(
        core.command(Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[1]],
            expected_revision: token,
        })
        .is_err()
    );
    assert!(
        core.command(Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[1]],
            before_entry_id: None,
            after: false,
            expected_revision: reordered.playlist_entries.unwrap().revision,
        })
        .is_err()
    );
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4]);
    assert_eq!(core.state.playlist_query_revisions(1), removed);
}

#[test]
fn playlist_location_batches_publish_scoped_tokens_even_for_empty_results() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "mix".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let entry = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    let query = Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 1,
        entry_ids: vec![entry, i64::MAX],
    };
    let before = core.query_response(&query);
    assert_eq!(before.revisions, core.state.playlist_query_revisions(1));
    assert!(
        matches!(before.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.len() == 1 && rows[0].position == 0)
    );
    let empty = core.query_response(&Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 1,
        entry_ids: vec![i64::MAX],
    });
    assert_eq!(empty.revisions, before.revisions);
    assert!(
        matches!(empty.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.is_empty())
    );
    let oversized = core.query_response(&Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 1,
        entry_ids: vec![entry; MAX_QUERY_ROWS + 1],
    });
    assert_eq!(oversized.revisions, before.revisions);
    assert!(matches!(oversized.result, Err(QueryError::Message(_))));
    core.command(Command::AddPlaylist {
        playlist_id: 1,
        track_ids: vec![3],
    })
    .unwrap();
    assert!(
        core.command(Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![entry],
            before_entry_id: None,
            after: false,
            expected_revision: before.revisions.playlist_entries.unwrap().revision,
        })
        .is_err()
    );
    assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [1, 2, 3]);
    let current = core.query_response(&query);
    assert_ne!(
        current.revisions.playlist_entries,
        before.revisions.playlist_entries
    );
    assert!(
        matches!(current.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows[0].total == 3)
    );
}

#[test]
fn playlist_append_and_move_only_invalidate_changed_pages() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "mix".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let entry_id = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    let expected = core.state.playlist_query_revisions(1);
    let query = Query::PlaylistEntryBatch {
        query: None,
        playlist_id: 1,
        after: None,
        limit: 1,
        expected: Some(expected),
    };
    for command in [
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![],
        },
        Command::AddPlaylistSources {
            other_tracks: false,
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![],
        },
        Command::MovePlaylistEntry { entry_id, index: 0 },
    ] {
        core.command(command).unwrap();
        assert_eq!(core.state.playlist_query_revisions(1), expected);
        assert!(core.view(&query).is_ok());
    }
    for command in [
        Command::AddPlaylist {
            playlist_id: 999,
            track_ids: vec![],
        },
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![3, 999],
        },
        Command::AddPlaylistSources {
            other_tracks: false,
            playlist_id: 999,
            directories: vec![],
            track_ids: vec![],
        },
        Command::AddPlaylistSources {
            other_tracks: false,
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![3, 999],
        },
        Command::MovePlaylistEntry {
            entry_id,
            index: usize::MAX,
        },
        Command::MovePlaylistEntry {
            entry_id: i64::MAX,
            index: 0,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.state.playlist_query_revisions(1), expected);
        assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [1, 2]);
    }
    for command in [
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![3],
        },
        Command::AddPlaylistSources {
            other_tracks: false,
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![4],
        },
        Command::MovePlaylistEntry { entry_id, index: 3 },
    ] {
        let reordered = matches!(command, Command::MovePlaylistEntry { .. });
        let before = core.state.playlist_query_revisions(1);
        let query = Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 1,
            expected: Some(before),
        };
        core.command(command).unwrap();
        let after = core.state.playlist_query_revisions(1);
        if reordered {
            assert_eq!(after.playlist, before.playlist);
            assert_ne!(after.playlist_entries, before.playlist_entries);
        } else {
            assert_ne!(after.playlist, before.playlist);
        }
        assert_eq!(after.library, before.library);
        assert_eq!(after.queue, before.queue);
        assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
    }
    assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [2, 3, 4, 1]);
    for command in [
        Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![2, 1, 2],
        },
        Command::AddPlaylistSources {
            other_tracks: false,
            playlist_id: 1,
            directories: vec![],
            track_ids: vec![1, 2],
        },
    ] {
        let before = core.state.playlist_query_revisions(1);
        core.command(command).unwrap();
        assert_ne!(
            core.state.playlist_query_revisions(1).playlist_entries,
            before.playlist_entries
        );
    }
    assert_eq!(
        core.store.playlist_track_ids(1, 20).unwrap(),
        [2, 3, 4, 1, 2, 1, 2, 1, 2]
    );
}

#[test]
fn playlist_reorders_preserve_catalog_and_other_playlist_queries() {
    let (_directory, mut core) = fixture();
    for (name, tracks) in [("one", vec![1, 2, 3]), ("two", vec![2, 3, 4])] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids: tracks,
        })
        .unwrap();
    }
    core.publish();
    let before = core.state.revisions;
    let summaries = Query::PlaylistSummaries {
        query: None,
        offset: 0,
        limit: 2,
    };
    let catalog = core.query_response(&summaries);
    let first_entry = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    let old_lookup = core.query_response(&Query::PlaylistEntry {
        entry_id: first_entry,
    });
    let batch = |playlist_id, after, expected| Query::PlaylistEntryBatch {
        query: None,
        playlist_id,
        after,
        limit: 1,
        expected,
    };
    let first = core.query_response(&batch(1, None, Some(catalog.revisions)));
    let other = core.query_response(&batch(2, None, Some(catalog.revisions)));
    let Ok(ViewResponse::PlaylistEntryBatch(entries)) = &first.result else {
        panic!("expected first entry batch");
    };
    let continuation = entries.next;
    let old_snapshot = GuiSnapshot::from_core(&core.state);
    core.command(Command::MovePlaylistEntry {
        entry_id: first_entry,
        index: 2,
    })
    .unwrap();
    core.publish();
    assert_eq!(
        core.state.revisions.changed_since(before),
        StateSections {
            library: true,
            ..StateSections::default()
        }
    );
    assert_eq!(
        serde_json::to_value(core.query_response(&summaries)).unwrap(),
        serde_json::to_value(&catalog).unwrap(),
    );
    assert_eq!(old_snapshot.library.playlist_entry_revisions.len(), 2);
    assert_eq!(core.state.library.playlist_entry_revisions.len(), 2);
    assert_ne!(
        old_snapshot.library.playlist_entry_revisions.get(&1),
        core.state.library.playlist_entry_revisions.get(&1)
    );
    assert_eq!(
        old_snapshot.library.playlist_entry_revisions.get(&2),
        core.state.library.playlist_entry_revisions.get(&2)
    );
    assert_eq!(core.state.query_revisions(), catalog.revisions);
    assert_eq!(
        core.view(&batch(1, continuation, Some(first.revisions)))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(core.view(&batch(2, None, Some(other.revisions))).is_ok());
    // A summary baseline cannot silently continue an entry traversal.
    assert_eq!(
        core.view(&batch(1, continuation, Some(catalog.revisions)))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let fresh = core.query_response(&Query::PlaylistEntry {
        entry_id: first_entry,
    });
    assert_ne!(
        fresh.revisions.playlist_entries,
        old_lookup.revisions.playlist_entries
    );
    assert!(
        matches!(fresh.result, Ok(ViewResponse::PlaylistEntry(Some(entry)))
        if entry.id == first_entry && entry.position == 2)
    );
    assert!(
        core.view(&Query::PlaylistSummaryBatch {
            query: None,
            after: Some(1),
            limit: 1,
            expected: Some(catalog.revisions),
        })
        .is_ok()
    );
    assert!(
        core.view(&Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 1,
            expected: Some(catalog.revisions),
        })
        .is_ok()
    );

    let current = core.state.playlist_query_revisions(1);
    let other_entry = core
        .store
        .playlist_entries_page(2, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    // Two mutations may coalesce into one snapshot; both scoped tokens survive.
    for index in [2, 1] {
        core.command(Command::MovePlaylistEntry {
            entry_id: other_entry,
            index,
        })
        .unwrap();
    }
    core.publish();
    assert!(core.view(&batch(1, None, Some(current))).is_ok());
    assert_eq!(
        core.view(&batch(2, None, Some(other.revisions)))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert_eq!(core.state.library.playlist_entry_revisions.len(), 2);
    assert_eq!(core.state.query_revisions(), catalog.revisions);
    // A token for another playlist is never an entry-order baseline.
    assert_eq!(
        core.view(&batch(2, None, Some(current))).unwrap_err(),
        QueryError::RevisionChanged
    );
}

#[test]
fn playlist_catalog_changes_preserve_unrelated_content_and_metadata_has_its_own_stamp() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "one".into(),
        track_ids: vec![1, 2, 3],
    })
    .unwrap();
    let removed = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    for (command, total) in [
        (Command::CreatePlaylist { name: "two".into() }, 2),
        (
            Command::RenamePlaylist {
                playlist_id: 1,
                name: "renamed".into(),
            },
            2,
        ),
        (
            Command::AddPlaylist {
                playlist_id: 1,
                track_ids: vec![4],
            },
            2,
        ),
        (Command::RemovePlaylistEntry { entry_id: removed }, 2),
        (Command::DeletePlaylist { playlist_id: 1 }, 1),
    ] {
        let page = core.store.playlist_entries_page(1, None, 0, 1).unwrap();
        core.command(Command::MovePlaylistEntry {
            entry_id: page.rows[0].id,
            index: page.total - 1,
        })
        .unwrap();
        let expected = core.state.playlist_query_revisions(1);
        let content_changed = matches!(
            command,
            Command::AddPlaylist { .. }
                | Command::RemovePlaylistEntry { .. }
                | Command::DeletePlaylist { .. }
        );
        core.command(command).unwrap();
        assert_eq!(core.state.library.playlist_total, total);
        assert_ne!(core.state.library.playlist_revision, expected.playlist);
        assert_eq!(core.state.library.revision, expected.library);
        let current = core.state.playlist_query_revisions(1);
        assert_eq!(
            current.duplicate_members != expected.duplicate_members,
            content_changed
        );
        assert_eq!(
            core.state.library.playlist_entry_revisions.len(),
            core.store.playlist_summary_page(None, 0, 0).unwrap().total
        );
        let query = Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 1,
            expected: Some(expected),
        };
        if content_changed {
            assert_ne!(current.playlist_entries, expected.playlist_entries);
            assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
        } else {
            assert_eq!(current.playlist_entries, expected.playlist_entries);
            assert!(core.view(&query).is_ok());
        }
    }
    core.command(Command::AddPlaylist {
        playlist_id: 2,
        track_ids: vec![1, 2],
    })
    .unwrap();
    let expected = core.state.playlist_query_revisions(2);
    core.command(Command::EditTrack {
        track_id: 1,
        title: Some("edited".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert_ne!(core.state.library.revision, expected.library);
    assert_eq!(
        core.view(&Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 2,
            after: None,
            limit: 1,
            expected: Some(expected),
        })
        .unwrap_err(),
        QueryError::RevisionChanged
    );
    let response = core.query_response(&Query::PlaylistEntries {
        query: None,
        playlist_id: 2,
        offset: 0,
        limit: 1,
    });
    assert!(
        matches!(response.result, Ok(ViewResponse::PlaylistEntries(page))
        if page.rows[0].title == "edited")
    );
}

#[test]
fn playlist_lifecycles_only_invalidate_member_facts_for_committed_members() {
    for (command, has_members, empty_queue) in [
        (
            Command::CreatePlaylist {
                name: "empty".into(),
            },
            false,
            false,
        ),
        (
            Command::CreatePlaylistWithTracks {
                name: "empty tracks".into(),
                track_ids: vec![],
            },
            false,
            false,
        ),
        (
            Command::CreatePlaylistWithTracks {
                name: "tracks".into(),
                track_ids: vec![1, 2, 1],
            },
            true,
            false,
        ),
        (
            Command::CreatePlaylistFromQueue {
                name: "queue".into(),
            },
            true,
            false,
        ),
        (
            Command::CreatePlaylistFromQueue {
                name: "empty queue".into(),
            },
            false,
            true,
        ),
    ] {
        let (_directory, mut core) = fixture();
        core.command(Command::CreatePlaylistWithTracks {
            name: "unaffected".into(),
            track_ids: vec![3, 4],
        })
        .unwrap();
        if empty_queue {
            core.command(Command::ClearQueue).unwrap();
        }
        let unaffected = core.state.playlist_query_revisions(1);
        let other_query = Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 2,
            expected: Some(unaffected),
        };
        let members_query = Query::DuplicateMembers {
            fingerprint: blake3::hash(b"lifecycle facts").to_hex().to_string(),
            offset: 0,
            limit: 10,
        };
        for invalid in [
            Command::CreatePlaylistWithTracks {
                name: "invalid track".into(),
                track_ids: vec![1, i64::MAX],
            },
            Command::DeletePlaylist {
                playlist_id: i64::MAX,
            },
            Command::RenamePlaylist {
                playlist_id: i64::MAX,
                name: "invalid".into(),
            },
        ] {
            assert!(core.command(invalid).is_err());
            assert_eq!(core.state.playlist_query_revisions(1), unaffected);
            assert_eq!(core.state.library.playlist_total, 1);
            assert!(core.view(&other_query).is_ok());
        }
        let playlist_query = Query::PlaylistEntries {
            query: None,
            playlist_id: 2,
            offset: 0,
            limit: 10,
        };
        let members_before = core.query_response(&members_query);
        assert!(members_before.result.is_ok());
        let before = members_before.revisions;
        core.command(command).unwrap();
        let created = core.query_response(&playlist_query);
        let Ok(ViewResponse::PlaylistEntries(page)) = created.result else {
            panic!("expected committed playlist");
        };
        assert_eq!(page.total != 0, has_members);
        assert!(created.revisions.playlist_entries.is_some());
        assert_eq!(core.state.library.playlist_total, 2);
        for deleting in [false, true] {
            let previous = if deleting {
                core.command(Command::DeletePlaylist { playlist_id: 2 })
                    .unwrap();
                created.revisions
            } else {
                before
            };
            let members_after = core.query_response(&members_query);
            assert!(members_after.result.is_ok());
            let after = members_after.revisions;
            assert_eq!(after.library, previous.library);
            assert_eq!(after.structure, previous.structure);
            assert_eq!(after.queue, previous.queue);
            assert_eq!(after.duplicate_groups, previous.duplicate_groups);
            assert_ne!(after.playlist, previous.playlist);
            assert_eq!(
                after.duplicate_members != previous.duplicate_members,
                has_members
            );
            assert_eq!(
                members_query.stamp(after) != members_query.stamp(previous),
                has_members
            );
            let other = core.query_response(&other_query);
            assert!(other.result.is_ok());
            assert_eq!(
                other.revisions.playlist_entries,
                unaffected.playlist_entries
            );
        }
        let deleted = core.query_response(&playlist_query);
        assert!(deleted.result.is_err());
        assert!(deleted.revisions.playlist_entries.is_none());
        assert_eq!(core.state.library.playlist_total, 1);
    }
}

#[test]
fn album_commands_resolve_all_members_and_reject_stale_keys_before_stop() {
    let (directory, mut core) = fixture();
    let records = (0..MAX_QUERY_ROWS + 3)
        .map(|number| ScanRecord {
            path: directory.path().join(format!("album-{number}.wav")),
            size: 1,
            modified_ns: 1,
            fingerprint: None,
            media: MediaInfo {
                title: format!("Track {number}"),
                artist: String::new(),
                album: "Large".into(),
                duration: Some(10.0),
                codec: "pcm".into(),
                channels: 2,
                sample_rate: 48_000,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: Some(number as u32 + 1),
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            },
        })
        .collect::<Vec<_>>();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            roots: vec![directory.path().into()],
            records: records.into_iter().map(std::sync::Arc::new).collect(),
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    let key = rivu_protocol::model::AlbumKey {
        directory: directory.path().into(),
        title: "Large".into(),
    };
    core.command(Command::EnqueueAlbum { key: key.clone() })
        .unwrap();
    assert_eq!(core.queue_ids.len(), MAX_QUERY_ROWS + 8);
    let first = core.store.album_tracks_page(&key, None, 0, 1).unwrap().rows[0].id;
    core.command(Command::CreatePlaylist {
        name: "whole album".into(),
    })
    .unwrap();
    let source = rivu_protocol::model::CollectionSource::Album {
        key: key.clone(),
        revision: core.state.library.album_revision,
    };
    core.command(Command::TransferCollection {
        source: source.clone(),
        destination: rivu_protocol::model::CollectionDestination::Playlist {
            playlist_id: 1,
            revision: core.state.library.playlist_entry_revisions[&1],
            index: None,
        },
    })
    .unwrap();
    assert_eq!(
        core.store
            .playlist_entries_page(1, None, 0, 0)
            .unwrap()
            .total,
        MAX_QUERY_ROWS + 3
    );
    core.command(Command::TransferCollection {
        source,
        destination: rivu_protocol::model::CollectionDestination::Queue { insertion: None },
    })
    .unwrap();
    assert_eq!(core.queue_ids.len(), 2 * (MAX_QUERY_ROWS + 3) + 5);
    write_pcm(&core.track(first).unwrap().path);
    let _commands = recording_audio(&mut core);
    core.command(Command::PlayAlbum { key }).unwrap();
    assert_eq!(core.queue_ids.len(), MAX_QUERY_ROWS + 3);
    assert_eq!(core.store.queue_track_id(core.queue_ids[0]).unwrap(), first);
    assert_eq!(core.state.queue.current_id, Some(core.queue_ids[0]));
    let queue = core.queue_ids.clone();
    let generation = core.generation;
    let current_id = core.state.queue.current_id;
    let playback_status = core.state.playback.status;
    for title in ["Absent", "", " \t\r\n", "\u{a0}\u{2003}\u{202f}\u{3000}"] {
        let key = rivu_protocol::model::AlbumKey {
            directory: directory.path().into(),
            title: title.into(),
        };
        for command in [
            Command::PlayAlbum { key: key.clone() },
            Command::EnqueueAlbum { key: key.clone() },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(core.queue_ids, queue);
            assert_eq!(core.generation, generation);
            assert_eq!(core.state.queue.current_id, current_id);
            assert_eq!(core.state.playback.status, playback_status);
        }
    }
}

#[test]
fn single_queue_removal_survives_restore() {
    let (_directory, mut core) = fixture();
    core.state.queue.current_id = Some(2);
    core.save_queue().unwrap();
    core.command(Command::RemoveQueue { queue_id: 2 }).unwrap();
    core.load_initial_state().unwrap();
    assert_eq!(queue_entries(&core), [(1, 1), (3, 2), (4, 3), (5, 4)]);
    assert_eq!(core.state.queue.current_id, None);
}

#[test]
fn playlist_creation_failure_leaves_catalog_and_versions_unchanged() {
    let (_directory, mut core) = fixture();
    core.publish();
    let before = core.state.query_revisions();
    assert!(
        core.command(Command::CreatePlaylistWithTracks {
            name: "invalid".into(),
            track_ids: vec![1, 999],
        })
        .is_err()
    );
    core.publish();
    assert_eq!(
        core.store.playlist_summary_page(None, 0, 1).unwrap().total,
        0
    );
    assert_eq!(core.state.library.playlist_total, 0);
    assert_eq!(core.state.query_revisions(), before);
    core.command(Command::CreatePlaylistWithTracks {
        name: "valid".into(),
        track_ids: vec![2, 1, 2],
    })
    .unwrap();
    let playlist = core.store.playlist_summary_page(None, 0, 1).unwrap().rows[0].id;
    assert_eq!(
        core.store.playlist_track_ids(playlist, 10).unwrap(),
        [2, 1, 2]
    );
    assert_eq!(core.state.library.playlist_total, 1);
}

#[test]
fn playlist_dedup_preserves_first_entries_and_is_scoped_revision_safe() {
    let (_directory, mut core) = fixture();
    for (name, track_ids) in [
        ("Repeated", vec![2, 1, 2, 3, 1]),
        ("Independent", vec![1, 1]),
    ] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids,
        })
        .unwrap();
    }
    let before = core
        .store
        .playlist_entries_page(1, None, 0, 10)
        .unwrap()
        .rows;
    let independent = core.store.playlist_entries_page(2, None, 0, 10).unwrap();
    let queue = core.store.queue_page(None, 0, 10).unwrap();
    let queue_revision = core.state.queue.revision;
    let revision = core.state.library.playlist_entry_revisions[&1];
    let independent_revision = core.state.library.playlist_entry_revisions[&2];
    core.command(Command::DeduplicatePlaylist {
        playlist_id: 1,
        expected_revision: revision,
    })
    .unwrap();
    let after = core
        .store
        .playlist_entries_page(1, None, 0, 10)
        .unwrap()
        .rows;
    assert_eq!(
        after.iter().map(|entry| entry.track_id).collect::<Vec<_>>(),
        [2, 1, 3]
    );
    assert_eq!(
        after.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        [before[0].id, before[1].id, before[3].id]
    );
    assert_eq!(
        after.iter().map(|entry| entry.position).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        core.store.playlist_entries_page(2, None, 0, 10).unwrap(),
        independent
    );
    assert_eq!(core.store.queue_page(None, 0, 10).unwrap(), queue);
    assert_eq!(core.state.queue.revision, queue_revision);
    assert_eq!(
        core.state.library.playlist_entry_revisions[&2],
        independent_revision
    );
    let deduplicated_revision = core.state.library.playlist_entry_revisions[&1];
    assert_ne!(deduplicated_revision, revision);
    assert!(
        core.command(Command::DeduplicatePlaylist {
            playlist_id: 1,
            expected_revision: revision,
        })
        .is_err()
    );
    core.command(Command::DeduplicatePlaylist {
        playlist_id: 1,
        expected_revision: deduplicated_revision,
    })
    .unwrap();
    assert_eq!(
        core.state.library.playlist_entry_revisions[&1],
        deduplicated_revision
    );
    assert!(
        core.command(Command::DeduplicatePlaylist {
            playlist_id: 0,
            expected_revision: 0,
        })
        .is_err()
    );
}

#[test]
fn interval_queue_moves_match_stable_selection_order_and_preserve_current() {
    let (_directory, mut core) = fixture();
    let original = core.queue_ids.clone();
    let current = original[2];
    core.store.set_queue_current(Some(current)).unwrap();
    for mask in 0..(1usize << original.len()) {
        for target in (0..=original.len()).chain(std::iter::once(usize::MAX)) {
            core.store.reorder_queue(&original).unwrap();
            core.queue_ids.clone_from(&original);
            core.refresh_queue_tracks().unwrap();
            let selected: Vec<_> = original
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, &id)| id)
                .collect();
            let mut expected: Vec<_> = original
                .iter()
                .copied()
                .filter(|id| !selected.contains(id))
                .collect();
            let insertion = target.min(expected.len());
            expected.splice(insertion..insertion, selected.iter().copied());
            let mut requested: Vec<_> = selected.iter().rev().copied().collect();
            if let Some(&id) = selected.first() {
                requested.push(id);
            }
            let revision = core.state.queue.revision;
            core.command(Command::MoveQueueEntries {
                queue_ids: requested,
                index: target,
            })
            .unwrap();
            assert_eq!(core.queue_ids, expected, "mask={mask} target={target}");
            assert_queue_saved(&core);
            assert_eq!(
                core.state.queue.revision,
                revision + u64::from(expected != original),
                "mask={mask} target={target}"
            );
            assert_eq!(core.state.queue.current_id, Some(current));
            assert_eq!(
                core.state.queue.current_position,
                expected.iter().position(|&id| id == current)
            );
            let ViewResponse::QueuePage(page) = core
                .view(&Query::QueuePage {
                    query: None,
                    offset: 0,
                    limit: MAX_QUERY_ROWS,
                })
                .unwrap()
            else {
                panic!("expected queue page")
            };
            assert_eq!(page.total, original.len());
            assert_eq!(
                page.rows
                    .iter()
                    .map(|row| (row.id, row.position))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .enumerate()
                    .map(|(position, &id)| (id, position))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn failed_queue_moves_preserve_memory_order_current_history_and_revision() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    playing(&mut core, 3);
    core.played = vec![1, 3, 5];
    core.played_cursor = 2;
    let order = core.queue_ids.clone();
    let state = core.state.queue.clone();
    let generation = core.generation;
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_queue_move BEFORE UPDATE OF position ON playlist_entries
             WHEN NEW.playlist_id=0 BEGIN SELECT RAISE(ABORT,'queue move rejected'); END;",
        )
        .unwrap();
    for command in [
        Command::MoveQueue {
            queue_id: 3,
            index: 0,
        },
        Command::MoveQueueEntries {
            queue_ids: vec![5, 1, 3, 1],
            index: 1,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(core.queue_ids, order);
        assert_eq!(core.state.queue, state);
        assert_queue_saved(&core);
        assert_eq!(core.generation, generation);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        assert_eq!(core.played, [1, 3, 5]);
        assert_eq!(core.played_cursor, 2);
    }
}

#[test]
fn bulk_queue_edits_preserve_current_order_history_and_reject_invalid_ids_atomically() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 3);
    progress(&mut core, 1.5, 1.5);
    core.played = vec![1, 3, 5];
    core.played_cursor = 2;
    let generation = core.generation;
    let before = queue_entries(&core);
    for command in [
        Command::MoveQueueEntries {
            queue_ids: vec![3, 99],
            index: 0,
        },
        Command::RemoveQueueEntries {
            queue_ids: vec![3, 99],
        },
        Command::MoveQueue {
            queue_id: 3,
            index: usize::MAX,
        },
    ] {
        assert!(core.command(command).is_err());
        assert_eq!(queue_entries(&core), before);
        assert_eq!(core.generation, generation);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    }
    core.command(Command::MoveQueueEntries {
        queue_ids: vec![5, 1, 3, 1],
        index: 1,
    })
    .unwrap();
    assert_eq!(
        queue_entries(&core),
        [(2, 2), (1, 1), (3, 2), (5, 4), (4, 3)]
    );
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(2));
    assert_eq!(core.generation, generation);
    assert_eq!(core.played, [1, 3, 5]);
    core.command(Command::MoveQueueEntries {
        queue_ids: vec![1, 3, 5],
        index: usize::MAX,
    })
    .unwrap();
    assert_eq!(
        queue_entries(&core),
        [(2, 2), (4, 3), (1, 1), (3, 2), (5, 4)]
    );
    core.command(Command::RemoveQueueEntries {
        queue_ids: vec![1, 1],
    })
    .unwrap();
    assert_eq!(core.generation, generation);
    assert_eq!(core.played, [3, 5]);
    assert_eq!(core.played_cursor, 1);
    core.command(Command::RemoveQueueEntries {
        queue_ids: vec![3, 3],
    })
    .unwrap();
    assert_eq!(core.state.queue.current_id, None);
    assert!(core.state.current_track().is_none());
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_queue_saved(&core);
}

#[test]
fn dedup_preserves_playing_duplicate_identity_at_first_position_and_prunes_history() {
    let (_directory, mut core) = fixture();
    core.command(Command::MoveQueue {
        queue_id: 3,
        index: 4,
    })
    .unwrap();
    playing(&mut core, 3);
    core.played = vec![1, 2, 4, 3, 2, 5];
    core.played_cursor = 4;
    let generation = core.generation;
    core.command(Command::DeduplicateQueue).unwrap();
    assert_eq!(queue_entries(&core), [(1, 1), (3, 2), (4, 3), (5, 4)]);
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.state.queue.current_position, Some(1));
    assert_eq!(core.played, [1, 4, 3, 5]);
    assert_eq!(core.played_cursor, 3);
    assert_eq!(core.generation, generation);
    assert_queue_saved(&core);
}

#[test]
fn randomize_changes_order_without_changing_current_or_history() {
    use rand::SeedableRng;
    let (_directory, mut core) = fixture();
    playing(&mut core, 3);
    core.played = vec![1, 3, 5];
    core.played_cursor = 2;
    let generation = core.generation;
    let original = core.queue_ids.clone();
    core.randomize_queue(&mut rand::rngs::StdRng::seed_from_u64(42))
        .unwrap();
    assert_ne!(core.queue_ids, original);
    assert_queue_saved(&core);
    let mut actual = core.queue_ids.clone();
    actual.sort_unstable();
    let mut expected = original;
    expected.sort_unstable();
    assert_eq!(actual, expected);
    assert_eq!(core.state.queue.current_id, Some(3));
    assert_eq!(core.played, [1, 3, 5]);
    assert_eq!(core.played_cursor, 2);
    assert_eq!(core.generation, generation);
}

#[test]
fn empty_and_singleton_queue_edits_are_valid_and_empty_bulk_requests_are_no_ops() {
    let (_directory, mut core) = fixture();
    let before = core.state.queue.clone();
    core.command(Command::RemoveQueueEntries { queue_ids: vec![] })
        .unwrap();
    core.command(Command::MoveQueueEntries {
        queue_ids: vec![],
        index: usize::MAX,
    })
    .unwrap();
    assert_eq!(core.state.queue, before);
    for singleton in [false, true] {
        core.command(Command::ClearQueue).unwrap();
        if singleton {
            core.enqueue(&[2]).unwrap();
        }
        let before = core.queue_ids.clone();
        core.command(Command::RandomizeQueue).unwrap();
        core.command(Command::DeduplicateQueue).unwrap();
        assert_eq!(core.queue_ids, before);
    }
}

#[test]
fn failed_playlist_start_does_not_keep_an_obsolete_queue_selection() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let playlist_id = core.store.create_playlist("missing").unwrap();
    core.store.add_playlist(playlist_id, &[2]).unwrap();
    core.playlist_created(playlist_id, true).unwrap();
    let error = core
        .command(Command::PlayPlaylist { playlist_id })
        .unwrap_err();
    assert!(error.to_string().contains("2.wav"));
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert_eq!(core.state.queue.current_id, None);
    missing(&mut core, Command::Resume, 2);
}

#[test]
fn collection_transfers_reject_source_changes_after_the_last_page() {
    use rivu_protocol::model::{AlbumKey, CollectionDestination, CollectionSource};
    let (_directory, mut core) = fixture();
    for track_id in [1, 2] {
        core.command(Command::EditTrack {
            track_id,
            title: None,
            artist: None,
            album: Some("Album".into()),
            decoder_hint: None,
        })
        .unwrap();
    }
    core.command(Command::CreatePlaylistWithTracks {
        name: "source".into(),
        track_ids: vec![2, 1],
    })
    .unwrap();
    core.command(Command::CreatePlaylistWithTracks {
        name: "target".into(),
        track_ids: vec![4],
    })
    .unwrap();
    let key = AlbumKey {
        directory: core.track(1).unwrap().path.parent().unwrap().into(),
        title: "Album".into(),
    };
    let album_source = CollectionSource::Album {
        key: key.clone(),
        revision: core.state.library.album_revision,
    };
    // Finish every page before changing the source; the submitted intent must
    // still be rejected rather than copy either the old IDs or the latest album.
    assert_eq!(
        core.store
            .album_tracks_page(&key, None, 0, 1)
            .unwrap()
            .rows
            .len(),
        1
    );
    assert_eq!(
        core.store
            .album_tracks_page(&key, None, 1, 1)
            .unwrap()
            .rows
            .len(),
        1
    );
    core.command(Command::EditTrack {
        track_id: 2,
        title: None,
        artist: None,
        album: Some("Other".into()),
        decoder_hint: None,
    })
    .unwrap();
    let playlist_source = CollectionSource::Playlist {
        playlist_id: 1,
        revision: core.state.library.playlist_entry_revisions[&1],
    };
    let last_entry = core
        .store
        .playlist_entries_page(1, None, 1, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::MovePlaylistEntry {
        entry_id: last_entry,
        index: 0,
    })
    .unwrap();
    let queue = core.queue_ids.clone();
    let queue_state = core.state.queue.clone();
    let target = core.store.playlist_track_ids(2, 20).unwrap();
    let target_revision = core.state.library.playlist_entry_revisions[&2];
    for source in [album_source, playlist_source] {
        for destination in [
            CollectionDestination::Queue { insertion: None },
            CollectionDestination::Playlist {
                playlist_id: 2,
                revision: target_revision,
                index: None,
            },
        ] {
            assert!(
                core.command(Command::TransferCollection {
                    source: source.clone(),
                    destination
                })
                .is_err()
            );
            assert_eq!(core.queue_ids, queue);
            assert_eq!(core.state.queue, queue_state);
            assert_eq!(core.store.playlist_track_ids(2, 20).unwrap(), target);
            assert_eq!(
                core.state.library.playlist_entry_revisions[&2],
                target_revision
            );
        }
    }
}

#[test]
fn collection_transfers_keep_source_order_and_existing_occurrences() {
    use rivu_protocol::model::{CollectionDestination, CollectionInsertion, CollectionSource};
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "source".into(),
        track_ids: vec![4, 2, 1],
    })
    .unwrap();
    let source = CollectionSource::Playlist {
        playlist_id: 1,
        revision: core.state.library.playlist_entry_revisions[&1],
    };
    let current = core.queue_ids[2];
    core.state.queue.current_id = Some(current);
    core.save_queue().unwrap();
    core.command(Command::TransferCollection {
        source: source.clone(),
        destination: CollectionDestination::Queue {
            insertion: Some(CollectionInsertion {
                index: 1,
                revision: core.state.queue.revision,
            }),
        },
    })
    .unwrap();
    assert_eq!(
        queue_entries(&core)
            .iter()
            .map(|&(_, track)| track)
            .collect::<Vec<_>>(),
        [1, 4, 2, 1, 2, 2, 3, 4]
    );
    assert_eq!(core.state.queue.current_id, Some(current));
    assert_eq!(core.state.queue.current_position, Some(5));
    core.command(Command::CreatePlaylistWithTracks {
        name: "target".into(),
        track_ids: vec![2, 3],
    })
    .unwrap();
    core.command(Command::TransferCollection {
        source,
        destination: CollectionDestination::Playlist {
            playlist_id: 2,
            revision: core.state.library.playlist_entry_revisions[&2],
            index: Some(1),
        },
    })
    .unwrap();
    assert_eq!(
        core.store.playlist_track_ids(2, 20).unwrap(),
        [2, 4, 2, 1, 3]
    );
}

#[test]
fn committed_playlist_creation_registers_tokens_and_refreshes_count() {
    for has_members in [false, true] {
        let (_directory, mut core) = fixture();
        let before = core.state.query_revisions();
        let command = if has_members {
            Command::CreatePlaylistWithTracks {
                name: "Committed".into(),
                track_ids: vec![2, 3],
            }
        } else {
            Command::CreatePlaylist {
                name: "Committed".into(),
            }
        };
        core.command(command).unwrap();
        assert_eq!(core.state.library.playlist_total, 1);
        assert!(core.state.library.playlist_entry_revisions.contains_key(&1));
        assert_ne!(core.state.library.playlist_entry_revisions[&1], 0);
        assert_eq!(core.state.library.playlist_revision, before.playlist + 1);
        assert_eq!(
            core.state.library.duplicate_members_revision,
            before.duplicate_members + u64::from(has_members)
        );
        assert_eq!(
            core.store.playlist_track_ids(1, 20).unwrap(),
            if has_members { vec![2, 3] } else { vec![] }
        );
    }
}

#[test]
fn stale_queue_recovery_retries_projection_and_prunes_shuffle_history_without_replacing_audio() {
    let (directory, mut core) = fixture();
    let track = core.track(1).unwrap();
    write_pcm_frames(&track.path, 1);
    let metadata = std::fs::metadata(&track.path).unwrap();
    let media = core
        .store
        .known_source(&track.path)
        .unwrap()
        .unwrap()
        .media
        .unwrap();
    let impacts = core
        .store
        .apply_scan(&ScanResult {
            records: vec![Arc::new(ScanRecord {
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
            })],
            ..ScanResult::default()
        })
        .unwrap();
    core.apply_scan_impacts(impacts).unwrap();
    pending(&mut core, 1);
    let source = core.playback.as_ref().unwrap().source.as_ref().unwrap();
    let source_facts = (
        source.0.id,
        source.0.path.clone(),
        source.0.file_size,
        source.0.modified_ns,
    );
    core.played = vec![1, 2, 3, 4, 5];
    core.played_cursor = 4;
    let connection = committed_queue_id_refresh_failure(&mut core, directory.path());
    let generation = core.generation;
    let revision = core.state.queue.revision;
    let commands = recording_audio(&mut core);
    assert!(core.ensure_queue_control_ready().is_err());
    assert!(core.queue_ids_stale);
    assert_eq!(core.played, [1, 2, 3, 4, 5]);
    repair_queue_ids(&connection);
    connection
        .execute_batch("UPDATE tracks SET channels='unreadable' WHERE id=1;")
        .unwrap();
    // This raw SQL fault bypasses Store's mutation-owned cache invalidation.
    // Evict the surviving track so recovery actually decodes the corrupt row.
    core.store.invalidate_caches();
    assert!(core.ensure_queue_control_ready().is_err());
    assert!(core.queue_ids_stale);
    assert_eq!(core.queue_ids, [1, 4, 5]);
    assert_eq!(core.played, [1, 4, 5]);
    assert_eq!(core.played_cursor, 2);
    connection
        .execute_batch("UPDATE tracks SET channels=2 WHERE id=1;")
        .unwrap();
    core.ensure_queue_control_ready().unwrap();
    assert!(!core.queue_ids_stale);
    assert_eq!(core.state.queue.total, 3);
    assert_eq!(core.state.queue.current_id, Some(1));
    assert_eq!(core.state.current_track().unwrap().id, 1);
    assert_eq!(core.state.queue.revision, revision);
    assert_eq!(core.generation, generation);
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.playback.as_ref().unwrap().track_id, 1);
    let source = core.playback.as_ref().unwrap().source.as_ref().unwrap();
    assert_eq!(
        (
            source.0.id,
            source.0.path.clone(),
            source.0.file_size,
            source.0.modified_ns
        ),
        source_facts
    );
    assert!(commands.try_recv().is_err());
}

#[test]
fn stale_queue_does_not_block_pause_toggle_pause_or_stop() {
    let (directory, mut core) = fixture();
    pending(&mut core, 1);
    let _connection = committed_queue_id_refresh_failure(&mut core, directory.path());
    let _commands = recording_audio(&mut core);
    core.command(Command::Pause).unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    // Resume of an existing owner is independent of queue identity recovery.
    core.command(Command::Toggle).unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    core.command(Command::Toggle).unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
    core.command(Command::Stop).unwrap();
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    assert!(core.queue_ids_stale);
}

#[test]
fn incremental_queue_commits_recover_projection_before_previous() {
    let cases = [
        ("INSERT", Command::Enqueue { track_ids: vec![1] }),
        (
            "INSERT",
            Command::EnqueueSources {
                other_tracks: false,
                directories: vec![],
                track_ids: vec![1],
            },
        ),
        (
            "INSERT",
            Command::InsertQueue {
                track_ids: vec![1],
                index: 0,
                expected_revision: 0,
            },
        ),
        (
            "INSERT",
            Command::InsertQueueSources {
                other_tracks: false,
                directories: vec![],
                track_ids: vec![1],
                index: 0,
                expected_revision: 0,
            },
        ),
        (
            "DELETE",
            Command::RemoveQueueEntries {
                queue_ids: vec![1, 2],
            },
        ),
        (
            "UPDATE OF position",
            Command::MoveQueueEntries {
                queue_ids: vec![5],
                index: 0,
            },
        ),
        ("UPDATE OF position", Command::RandomizeQueue),
    ];
    for (event, command) in cases {
        let (directory, mut core) = fixture();
        core.state.queue.current_id = Some(5);
        core.save_queue().unwrap();
        core.refresh_queue_tracks().unwrap();
        let command = match command {
            Command::InsertQueue {
                track_ids, index, ..
            } => Command::InsertQueue {
                track_ids,
                index,
                expected_revision: core.state.queue.revision,
            },
            Command::InsertQueueSources {
                other_tracks,
                directories,
                track_ids,
                index,
                ..
            } => Command::InsertQueueSources {
                other_tracks,
                directories,
                track_ids,
                index,
                expected_revision: core.state.queue.revision,
            },
            command => command,
        };
        let connection = fail_queue_projection_after(directory.path(), event);
        let commands = recording_audio(&mut core);
        // Seed randomization so the mutation is deterministic, not a possible no-op.
        if matches!(command, Command::RandomizeQueue) {
            use rand::SeedableRng;
            assert!(
                core.randomize_queue(&mut rand::rngs::StdRng::seed_from_u64(42))
                    .is_err()
            );
        } else {
            assert!(core.command(command).is_err());
        }
        assert!(core.navigate(Navigation::Previous).is_err());
        assert!(commands.try_recv().is_err());
        repair_queue_projection(&connection);
        let ids = core.store.queue_ids().unwrap();
        let position = ids.iter().position(|&id| id == 5).unwrap();
        let expected = ids[position.saturating_sub(1)];
        core.navigate(Navigation::Previous).unwrap();
        assert_eq!(core.state.queue.current_id, Some(expected));
        assert_eq!(core.state.queue.total, ids.len());
        assert_eq!(core.queue_ids, ids);
        assert_queue_saved(&core);
    }
}

#[test]
fn removal_projection_failure_recovers_two_entry_position_and_shuffle_history() {
    for shuffle in [false, true] {
        let (directory, mut core) = fixture();
        core.command(Command::RemoveQueue { queue_id: 5 }).unwrap();
        core.state.queue.current_id = Some(4);
        core.save_queue().unwrap();
        core.refresh_queue_tracks().unwrap();
        core.state.playback.shuffle = shuffle;
        core.played = vec![3, 1, 2, 4];
        core.played_cursor = 4;
        let connection = fail_queue_projection_after(directory.path(), "DELETE");
        let _commands = recording_audio(&mut core);
        assert!(
            core.command(Command::RemoveQueueEntries {
                queue_ids: vec![1, 2]
            })
            .is_err()
        );
        assert_eq!(core.store.queue_ids().unwrap(), [3, 4]);
        assert!(core.navigate(Navigation::Previous).is_err());
        repair_queue_projection(&connection);
        core.navigate(Navigation::Previous).unwrap();
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.state.queue.current_position, Some(0));
        assert_eq!(core.state.queue.total, 2);
        assert!(!core.played.contains(&1));
        assert!(!core.played.contains(&2));
        assert_queue_saved(&core);
    }
}

#[test]
fn failed_incremental_id_read_reloads_occurrences_before_navigation() {
    let (directory, mut core) = fixture();
    core.state.queue.current_id = Some(5);
    core.save_queue().unwrap();
    core.refresh_queue_tracks().unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER corrupt_appended_id AFTER INSERT ON playlist_entries
         WHEN NEW.playlist_id=0 BEGIN UPDATE playlist_entries SET id=-NEW.id WHERE id=NEW.id; END;",
        )
        .unwrap();
    assert!(
        core.command(Command::Enqueue { track_ids: vec![1] })
            .is_err()
    );
    let _commands = recording_audio(&mut core);
    assert!(core.navigate(Navigation::Previous).is_err());
    connection
        .execute_batch(
            "DROP TRIGGER corrupt_appended_id;
         UPDATE playlist_entries SET id=-id WHERE playlist_id=0 AND id<0;",
        )
        .unwrap();
    core.navigate(Navigation::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(4));
    assert_eq!(core.queue_ids, [1, 2, 3, 4, 5, 6]);
    assert_queue_saved(&core);
}
