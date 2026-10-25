use super::*;
use rivu_protocol::model::{
    AlbumKey, CollectionDestination, CollectionInsertion, CollectionSource,
};

fn album_query(core: &Core) -> Query {
    Query::AlbumTracks {
        key: AlbumKey {
            directory: core.track(1).unwrap().path.parent().unwrap().into(),
            title: "Album".into(),
        },
        query: None,
        offset: 0,
        limit: 20,
    }
}

fn playlist_query(playlist_id: i64) -> Query {
    Query::PlaylistEntries {
        playlist_id,
        query: None,
        offset: 0,
        limit: 20,
    }
}

fn prepare(core: &mut Core) {
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
    for (name, track_ids) in [
        ("source", vec![2, 1]),
        ("target", vec![4]),
        ("unrelated", vec![3]),
    ] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids,
        })
        .unwrap();
    }
}

#[test]
fn external_source_commits_reject_collection_transfers_without_destination_changes() {
    for album in [true, false] {
        let (directory, mut core) = fixture_with_database(Some(10.0), true);
        prepare(&mut core);
        let query = if album {
            album_query(&core)
        } else {
            playlist_query(1)
        };
        let response = core.query_response(&query);
        response.result.unwrap();
        let source = if let Query::AlbumTracks { key, .. } = query {
            CollectionSource::Album {
                key,
                revision: response.revisions.album,
            }
        } else {
            CollectionSource::Playlist {
                playlist_id: 1,
                revision: response.revisions.playlist_entries.unwrap().revision,
            }
        };
        let queue = core.store.queue_page(None, 0, 20).unwrap();
        let queue_state = core.store.queue_state().unwrap();
        let target = core.store.playlist_entries_page(2, None, 0, 20).unwrap();
        let target_revision = core
            .query_response(&playlist_query(2))
            .revisions
            .playlist_entries
            .unwrap()
            .revision;
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection
            .execute_batch(if album {
                "UPDATE tracks SET track_number=CASE id WHEN 1 THEN 2 ELSE 1 END WHERE id IN(1,2);"
            } else {
                "UPDATE playlist_entries SET position=1-position WHERE playlist_id=1;"
            })
            .unwrap();
        // The second writer commits before Core acquires BEGIN IMMEDIATE. Core's
        // published memory is deliberately still the selected, now stale snapshot.
        for destination in [
            CollectionDestination::Queue {
                insertion: Some(CollectionInsertion {
                    index: 1,
                    revision: queue_state.revision,
                }),
            },
            CollectionDestination::Playlist {
                playlist_id: 2,
                revision: target_revision,
                index: Some(0),
            },
        ] {
            assert!(
                core.command(Command::TransferCollection {
                    source: source.clone(),
                    destination
                })
                .is_err()
            );
            assert_eq!(core.store.queue_page(None, 0, 20).unwrap(), queue);
            assert_eq!(core.store.queue_state().unwrap(), queue_state);
            assert_eq!(
                core.store.playlist_entries_page(2, None, 0, 20).unwrap(),
                target
            );
            assert_eq!(
                core.store.playlist_entry_revision(2).unwrap(),
                Some(target_revision)
            );
        }
    }
}

#[test]
fn collection_queries_use_committed_versions_and_rollback_preserves_tokens() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    prepare(&mut core);
    let album = album_query(&core);
    let before_album = core.query_response(&album);
    let before_source = core.query_response(&playlist_query(1));
    let before_target = core.query_response(&playlist_query(2));
    let before_queue = core.query_response(&Query::QueuePage {
        query: None,
        offset: 0,
        limit: 20,
    });
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
        UPDATE tracks SET track_number=CASE id WHEN 1 THEN 2 ELSE 1 END WHERE id IN(1,2);
        UPDATE playlist_entries SET position=1-position WHERE playlist_id=1;",
        )
        .unwrap();
    let pending = core.query_response(&playlist_query(1));
    assert_eq!(
        pending.revisions.playlist_entries,
        before_source.revisions.playlist_entries
    );
    match (&pending.result, &before_source.result) {
        (Ok(ViewResponse::PlaylistEntries(pending)), Ok(ViewResponse::PlaylistEntries(before))) => {
            assert_eq!(pending, before)
        }
        _ => panic!("Expected playlist pages"),
    }
    connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        core.query_response(&album).revisions.album,
        before_album.revisions.album
    );
    assert_eq!(
        core.query_response(&playlist_query(1))
            .revisions
            .playlist_entries,
        before_source.revisions.playlist_entries
    );
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
        UPDATE tracks SET track_number=CASE id WHEN 1 THEN 2 ELSE 1 END WHERE id IN(1,2);
        UPDATE playlist_entries SET position=1-position WHERE playlist_id=1;
        COMMIT;",
        )
        .unwrap();
    let after_album = core.query_response(&album);
    assert_ne!(after_album.revisions.album, before_album.revisions.album);
    assert_ne!(
        album.stamp(after_album.revisions),
        album.stamp(before_album.revisions)
    );
    let Ok(ViewResponse::AlbumTracks(page)) = after_album.result else {
        panic!("Expected album page")
    };
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [2, 1]
    );
    let after_source = core.query_response(&playlist_query(1));
    assert_ne!(
        after_source.revisions.playlist_entries,
        before_source.revisions.playlist_entries
    );
    let Ok(ViewResponse::PlaylistEntries(page)) = after_source.result else {
        panic!("Expected playlist page")
    };
    assert_eq!(
        page.rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        core.query_response(&playlist_query(2))
            .revisions
            .playlist_entries,
        before_target.revisions.playlist_entries
    );
    assert_eq!(
        core.query_response(&Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20
        })
        .revisions
        .queue,
        before_queue.revisions.queue
    );
    assert_eq!(
        core.query_response(&Query::PlaylistEntryBatch {
            playlist_id: 1,
            query: None,
            after: None,
            limit: 1,
            expected: Some(before_source.revisions),
        })
        .result
        .unwrap_err(),
        QueryError::RevisionChanged
    );
}

#[test]
fn external_destination_commits_and_playlist_aba_reject_stale_collection_intents() {
    for (playlist_id, aba) in [(2, false), (2, true), (1, true)] {
        let (directory, mut core) = fixture_with_database(Some(10.0), true);
        prepare(&mut core);
        let source_revision = core
            .query_response(&playlist_query(1))
            .revisions
            .playlist_entries
            .unwrap()
            .revision;
        let target_revision = core
            .query_response(&playlist_query(2))
            .revisions
            .playlist_entries
            .unwrap()
            .revision;
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        if aba {
            connection.execute_batch(&format!("BEGIN IMMEDIATE;
                DELETE FROM playlist_entries WHERE playlist_id={playlist_id};
                DELETE FROM playlists WHERE id={playlist_id};
                INSERT INTO playlists(id,name) VALUES({playlist_id},'replacement');
                INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES({playlist_id},3,0);
                COMMIT;")).unwrap();
        } else {
            connection
                .execute_batch(
                    "INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(2,3,1)",
                )
                .unwrap();
        }
        let queue = core.store.queue_page(None, 0, 20).unwrap();
        let target = core.store.playlist_entries_page(2, None, 0, 20).unwrap();
        let destination = if playlist_id == 1 {
            CollectionDestination::Queue { insertion: None }
        } else {
            CollectionDestination::Playlist {
                playlist_id: 2,
                revision: target_revision,
                index: Some(0),
            }
        };
        assert!(
            core.command(Command::TransferCollection {
                source: CollectionSource::Playlist {
                    playlist_id: 1,
                    revision: source_revision
                },
                destination,
            })
            .is_err()
        );
        assert_eq!(core.store.queue_page(None, 0, 20).unwrap(), queue);
        assert_eq!(
            core.store.playlist_entries_page(2, None, 0, 20).unwrap(),
            target
        );
        assert_ne!(
            core.store.playlist_entry_revision(playlist_id).unwrap(),
            Some(if playlist_id == 1 {
                source_revision
            } else {
                target_revision
            })
        );
    }
}

#[test]
fn collection_tokens_survive_reopen_and_unrelated_playlist_edits() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    prepare(&mut core);
    let source_revision = core
        .query_response(&playlist_query(1))
        .revisions
        .playlist_entries
        .unwrap()
        .revision;
    let album_revision = core.query_response(&album_query(&core)).revisions.album;
    let other = Store::open(&directory.path().join("library.db")).unwrap();
    assert_eq!(
        other.playlist_entry_revision(1).unwrap(),
        Some(source_revision)
    );
    assert_eq!(other.album_revision().unwrap(), album_revision);
    other.add_playlist(3, &[4]).unwrap();
    assert_eq!(
        core.query_response(&playlist_query(1))
            .revisions
            .playlist_entries
            .unwrap()
            .revision,
        source_revision
    );
    let before = core.store.queue_ids().unwrap().len();
    for _ in 0..2 {
        core.command(Command::TransferCollection {
            source: CollectionSource::Playlist {
                playlist_id: 1,
                revision: source_revision,
            },
            destination: CollectionDestination::Queue { insertion: None },
        })
        .unwrap();
    }
    let rows = core.store.queue_page(None, before, 20).unwrap().rows;
    assert_eq!(
        rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        [2, 1, 2, 1]
    );
    assert_eq!(
        core.store.playlist_entry_revision(1).unwrap(),
        Some(source_revision)
    );
}

#[test]
fn external_queue_commit_rejects_positional_collection_transfer() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    prepare(&mut core);
    let source_revision = core
        .query_response(&playlist_query(1))
        .revisions
        .playlist_entries
        .unwrap()
        .revision;
    let expected_queue = core
        .query_response(&Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20,
        })
        .revisions
        .queue;
    let other = Store::open(&directory.path().join("library.db")).unwrap();
    other.enqueue_tracks(&[3]).unwrap();
    let queue = other.queue_page(None, 0, 20).unwrap();
    let state = other.queue_state().unwrap();
    assert_ne!(
        core.query_response(&Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20
        })
        .revisions
        .queue,
        expected_queue
    );
    assert!(
        core.command(Command::TransferCollection {
            source: CollectionSource::Playlist {
                playlist_id: 1,
                revision: source_revision
            },
            destination: CollectionDestination::Queue {
                insertion: Some(CollectionInsertion {
                    index: 1,
                    revision: expected_queue
                })
            },
        })
        .is_err()
    );
    assert_eq!(other.queue_page(None, 0, 20).unwrap(), queue);
    assert_eq!(other.queue_state().unwrap(), state);
}

#[test]
fn external_playlist_query_publishes_observed_scope_before_reply() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    prepare(&mut core);
    core.publish();
    let before = core.shared.read().library.playlist_entry_revisions.clone();
    let (handle, control, worker, _) = navigation_runtime(core);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch("UPDATE playlist_entries SET position=1-position WHERE playlist_id=1")
        .unwrap();
    let query = playlist_query(1);
    let response = handle.query(query.clone());
    let snapshot = handle.gui_snapshot();
    assert_eq!(
        query.stamp(snapshot.query_revisions(Some(1))),
        query.stamp(response.revisions)
    );
    assert_ne!(snapshot.library.playlist_entry_revisions[&1], before[&1]);
    for id in [2, 3] {
        assert_eq!(snapshot.library.playlist_entry_revisions[&id], before[&id]);
    }
    let Ok(ViewResponse::PlaylistEntries(page)) = response.result else {
        panic!("Expected playlist entries")
    };
    assert_eq!(
        page.rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        [1, 2]
    );
    // A retry begins with the newly published authority, rather than discarding
    // every valid page against the original process-local token forever.
    let retried = handle.query(query.clone());
    let current = handle.gui_snapshot();
    assert_eq!(
        query.stamp(current.query_revisions(Some(1))),
        query.stamp(retried.revisions)
    );
    assert!(Arc::ptr_eq(
        &snapshot.library.playlist_entry_revisions,
        &current.library.playlist_entry_revisions
    ));
    connection
        .execute_batch("UPDATE playlist_entries SET position=1-position WHERE playlist_id=1")
        .unwrap();
    let lookup = handle.query(Query::PlaylistEntry {
        entry_id: page.rows[0].id,
    });
    let inferred = handle.gui_snapshot();
    assert!(lookup.result.is_ok());
    assert_eq!(
        inferred.query_revisions(Some(1)).playlist_entries,
        lookup.revisions.playlist_entries
    );
    assert_ne!(
        inferred.library.playlist_entry_revisions[&1],
        current.library.playlist_entry_revisions[&1]
    );
    connection
        .execute_batch("UPDATE playlist_entries SET position=1-position WHERE playlist_id=1")
        .unwrap();
    let mut epoch = QueryEpoch::default();
    let ticket = epoch.ticket();
    epoch.cancel();
    let (reply, receive) = oneshot::channel();
    handle
        .sender
        .blocking_send(Request {
            operation: Operation::Query {
                query: query.clone(),
                ticket: Some(ticket),
            },
            reply: Some(reply),
        })
        .unwrap();
    let CoreResponse::Query(cancelled) = wait_on(&executor(), receive).unwrap() else {
        panic!("Expected cancelled query")
    };
    assert_eq!(cancelled.result.unwrap_err(), QueryError::Cancelled);
    assert!(Arc::ptr_eq(
        &inferred.library.playlist_entry_revisions,
        &handle.gui_snapshot().library.playlist_entry_revisions
    ));
    connection
        .execute_batch("ALTER TABLE playlist_versions RENAME TO unavailable_playlist_versions")
        .unwrap();
    let failed = handle.query(query.clone());
    assert!(failed.result.is_err());
    assert!(Arc::ptr_eq(
        &inferred.library.playlist_entry_revisions,
        &handle.gui_snapshot().library.playlist_entry_revisions
    ));
    connection
        .execute_batch("ALTER TABLE unavailable_playlist_versions RENAME TO playlist_versions")
        .unwrap();
    connection
        .execute_batch(
            "DELETE FROM playlist_entries WHERE playlist_id=1; DELETE FROM playlists WHERE id=1;",
        )
        .unwrap();
    let deleted = handle.query(query);
    assert!(deleted.result.is_err());
    assert_eq!(deleted.revisions.playlist_entries, None);
    let current = handle.gui_snapshot();
    assert!(!current.library.playlist_entry_revisions.contains_key(&1));
    for id in [2, 3] {
        assert_eq!(current.library.playlist_entry_revisions[&id], before[&id]);
    }
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn external_playlist_entry_identity_change_rejects_stale_batch_cursor() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    prepare(&mut core);
    let first = core.query_response(&Query::PlaylistEntryBatch {
        playlist_id: 1,
        query: None,
        after: None,
        limit: 1,
        expected: None,
    });
    let Ok(ViewResponse::PlaylistEntryBatch(page)) = first.result else {
        panic!("Expected playlist batch")
    };
    let entry_id = page.rows[0].id;
    let after = page.next;
    assert!(after.is_some());
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute(
            "UPDATE playlist_entries SET id=id+1000 WHERE id=?1",
            [entry_id],
        )
        .unwrap();
    let continued = core.query_response(&Query::PlaylistEntryBatch {
        playlist_id: 1,
        query: None,
        after,
        limit: 1,
        expected: Some(first.revisions),
    });
    assert_eq!(continued.result.unwrap_err(), QueryError::RevisionChanged);
    assert_ne!(
        continued.revisions.playlist_entries,
        first.revisions.playlist_entries
    );
}

#[test]
fn external_album_query_publishes_observed_token_before_reply() {
    let (directory, mut core) = fixture_with_database(Some(10.0), true);
    prepare(&mut core);
    core.publish();
    let query = album_query(&core);
    let before = core.state.library.album_revision;
    let (handle, control, worker, _) = navigation_runtime(core);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "UPDATE tracks SET track_number=CASE id WHEN 1 THEN 2 ELSE 1 END WHERE id IN(1,2)",
        )
        .unwrap();
    let response = handle.query(query.clone());
    let snapshot = handle.gui_snapshot();
    assert_ne!(response.revisions.album, before);
    assert_eq!(
        query.stamp(snapshot.query_revisions(None)),
        query.stamp(response.revisions)
    );
    let Ok(ViewResponse::AlbumTracks(page)) = response.result else {
        panic!("Expected album tracks")
    };
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [2, 1]
    );
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}
