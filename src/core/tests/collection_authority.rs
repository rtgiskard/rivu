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
fn core_source_changes_reject_collection_transfers_without_destination_changes() {
    for album in [true, false] {
        let (_directory, mut core) = fixture();
        prepare(&mut core);
        let query = if album {
            album_query(&core)
        } else {
            playlist_query(1)
        };
        let response = core.query_response(&query);
        response.result.unwrap();
        let source = if let Query::AlbumTracks { key, .. } = query {
            core.command(Command::EditTrack {
                track_id: 1,
                title: None,
                artist: None,
                album: Some("Changed album".into()),
                decoder_hint: None,
            })
            .unwrap();
            CollectionSource::Album {
                key,
                revision: response.revisions.album,
            }
        } else {
            core.command(Command::AddPlaylist {
                playlist_id: 1,
                track_ids: vec![3],
            })
            .unwrap();
            CollectionSource::Playlist {
                playlist_id: 1,
                revision: response.revisions.playlist_entries.unwrap().revision,
            }
        };
        let queue = core.store.queue_page(None, 0, 20).unwrap();
        let queue_state = core.state.queue.clone();
        let target = core.store.playlist_entries_page(2, None, 0, 20).unwrap();
        let target_revision = core.state.library.playlist_entry_revisions[&2];
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
            assert_eq!(core.state.queue, queue_state);
            assert_eq!(
                core.store.playlist_entries_page(2, None, 0, 20).unwrap(),
                target
            );
            assert_eq!(
                core.state.library.playlist_entry_revisions[&2],
                target_revision
            );
        }
    }
}

#[test]
fn collection_transfer_rollback_preserves_all_tokens_and_rows() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    prepare(&mut core);
    let before = core.state.query_revisions();
    let scopes = core.state.library.playlist_entry_revisions.clone();
    let queue = core.store.queue_page(None, 0, 20).unwrap();
    let target = core.store.playlist_entries_page(2, None, 0, 20).unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_transfer BEFORE INSERT ON playlist_entries
         WHEN NEW.playlist_id=0 AND NEW.track_id=1 BEGIN SELECT RAISE(ABORT,'queue insertion refused'); END;
         CREATE TRIGGER reject_playlist_transfer BEFORE INSERT ON playlist_entries
         WHEN NEW.playlist_id=2 AND NEW.track_id=1
         BEGIN SELECT RAISE(ABORT,'playlist insertion refused'); END;",
        )
        .unwrap();
    for destination in [
        CollectionDestination::Queue { insertion: None },
        CollectionDestination::Playlist {
            playlist_id: 2,
            revision: scopes[&2],
            index: None,
        },
    ] {
        assert!(
            core.command(Command::TransferCollection {
                source: CollectionSource::Playlist {
                    playlist_id: 1,
                    revision: scopes[&1]
                },
                destination,
            })
            .is_err()
        );
        assert_eq!(core.state.query_revisions(), before);
        assert!(Arc::ptr_eq(
            &core.state.library.playlist_entry_revisions,
            &scopes
        ));
        assert_eq!(core.store.queue_page(None, 0, 20).unwrap(), queue);
        assert_eq!(
            core.store.playlist_entries_page(2, None, 0, 20).unwrap(),
            target
        );
    }
}

#[test]
fn core_destination_changes_reject_stale_collection_intents() {
    let (_directory, mut core) = fixture();
    prepare(&mut core);
    let source_revision = core.state.library.playlist_entry_revisions[&1];
    let target_revision = core.state.library.playlist_entry_revisions[&2];
    core.command(Command::AddPlaylist {
        playlist_id: 2,
        track_ids: vec![3],
    })
    .unwrap();
    let target = core.store.playlist_entries_page(2, None, 0, 20).unwrap();
    assert!(
        core.command(Command::TransferCollection {
            source: CollectionSource::Playlist {
                playlist_id: 1,
                revision: source_revision
            },
            destination: CollectionDestination::Playlist {
                playlist_id: 2,
                revision: target_revision,
                index: Some(0)
            },
        })
        .is_err()
    );
    assert_eq!(
        core.store.playlist_entries_page(2, None, 0, 20).unwrap(),
        target
    );
}

#[test]
fn collection_tokens_survive_unrelated_playlist_edits_and_repeated_transfers() {
    let (_directory, mut core) = fixture();
    prepare(&mut core);
    let source_revision = core.state.library.playlist_entry_revisions[&1];
    let album_revision = core.state.library.album_revision;
    core.command(Command::AddPlaylist {
        playlist_id: 3,
        track_ids: vec![4],
    })
    .unwrap();
    assert_eq!(
        core.state.library.playlist_entry_revisions[&1],
        source_revision
    );
    assert_eq!(core.state.library.album_revision, album_revision);
    let before = core.queue_ids.len();
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
        core.state.library.playlist_entry_revisions[&1],
        source_revision
    );
}

#[test]
fn core_queue_change_rejects_positional_collection_transfer() {
    let (_directory, mut core) = fixture();
    prepare(&mut core);
    let source_revision = core.state.library.playlist_entry_revisions[&1];
    let expected_queue = core.state.queue.revision;
    core.command(Command::Enqueue { track_ids: vec![3] })
        .unwrap();
    let queue = core.store.queue_page(None, 0, 20).unwrap();
    let state = core.state.queue.clone();
    assert_ne!(state.revision, expected_queue);
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
                }),
            },
        })
        .is_err()
    );
    assert_eq!(core.store.queue_page(None, 0, 20).unwrap(), queue);
    assert_eq!(core.state.queue, state);
}

#[test]
fn core_playlist_change_rejects_stale_batch_cursor() {
    let (_directory, mut core) = fixture();
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
    let after = page.next;
    assert!(after.is_some());
    core.command(Command::AddPlaylist {
        playlist_id: 1,
        track_ids: vec![3],
    })
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
fn core_playlist_recreation_never_reuses_a_stale_lifetime_token() {
    let (_directory, mut core) = fixture();
    prepare(&mut core);
    let old = core.state.playlist_query_revisions(3);
    let token = old.playlist_entries.unwrap().revision;
    core.command(Command::DeletePlaylist { playlist_id: 3 })
        .unwrap();
    assert!(!core.state.library.playlist_entry_revisions.contains_key(&3));
    core.command(Command::CreatePlaylistWithTracks {
        name: "replacement".into(),
        track_ids: vec![4],
    })
    .unwrap();
    let replacement_id = *core.store.playlist_ids().unwrap().last().unwrap();
    assert_ne!(replacement_id, 3);
    assert!(core.state.library.playlist_entry_revisions[&replacement_id] > token);
    let queue = core.queue_ids.clone();
    assert!(
        core.command(Command::TransferCollection {
            source: CollectionSource::Playlist {
                playlist_id: replacement_id,
                revision: token
            },
            destination: CollectionDestination::Queue { insertion: None },
        })
        .is_err()
    );
    assert_eq!(core.queue_ids, queue);
    assert_eq!(
        core.query_response(&Query::PlaylistEntryBatch {
            playlist_id: replacement_id,
            query: None,
            after: None,
            limit: 1,
            expected: Some(old),
        })
        .result
        .unwrap_err(),
        QueryError::RevisionChanged
    );
}
