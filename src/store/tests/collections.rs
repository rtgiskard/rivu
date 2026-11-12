use super::*;

#[test]
fn directory_selections_deduplicate_atomically_without_capping_playlists() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<5000) INSERT INTO tracks(path) SELECT '/music/' || CASE WHEN n<=2500 THEN 'A/' ELSE 'B/' END || n || '.wav' FROM ids").unwrap();
    let directories = vec![
        PathBuf::from("/music/A"),
        PathBuf::from("/music/B"),
        PathBuf::from("/music/A"),
    ];
    store.enqueue_sources(&directories, &[1, 1], None).unwrap();
    assert_eq!(store.queue_state().unwrap().total, 5000);
    assert_eq!(
        store.queue_page(None, 4999, 1).unwrap().rows[0].track_id,
        5000
    );
    let playlist = store.create_playlist("Unlimited").unwrap();
    store.add_playlist(playlist, &[5000]).unwrap();
    store
        .add_playlist_sources(playlist, &directories, &[1, 1], None)
        .unwrap();
    let first = store.playlist_entries_page(playlist, None, 0, 2).unwrap();
    assert_eq!(first.total, 5001);
    assert_eq!(
        first
            .rows
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [5000, 1]
    );
    let last = store
        .playlist_entries_page(playlist, None, 4999, 1)
        .unwrap();
    assert_eq!(last.rows[0].track_id, 4999);
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT MAX(position) FROM playlist_entries WHERE playlist_id=?1",
                [playlist],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        5000
    );
    store
        .add_playlist_sources(playlist, &directories, &[], None)
        .unwrap();
    assert_eq!(
        store
            .playlist_entries_page(playlist, None, 0, 0)
            .unwrap()
            .total,
        10001
    );
    let empty = store.create_playlist("Atomic").unwrap();
    assert!(
        store
            .add_playlist_sources(empty, &directories, &[99999], None)
            .is_err()
    );
    assert_eq!(
        store
            .playlist_entries_page(empty, None, 0, 0)
            .unwrap()
            .total,
        0
    );
}

#[test]
fn bulk_missing_cleanup_spans_pages_and_preserves_playlist_reordering() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<1000) INSERT INTO tracks(path,missing) SELECT '/music/'||n||'.wav',n<=500 FROM ids").unwrap();
    let playlist = store.create_playlist("Availability").unwrap();
    store.add_playlist(playlist, &[1000]).unwrap();
    store
        .add_playlist_sources(playlist, &[PathBuf::from("/music")], &[], None)
        .unwrap();
    store.mark_played(25, 42).unwrap();
    store.mark_played(1000, 43).unwrap();
    store.remove_missing_tracks().unwrap();
    assert_eq!(store.library_stats().unwrap().total, 500);
    assert_eq!(
        store
            .library_page(None, None, Some(true), 0, 1)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .history(200)
            .unwrap()
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [1000]
    );
    let page = store.playlist_entries_page(playlist, None, 0, 1).unwrap();
    assert_eq!(page.total, 501);
    assert_eq!(page.rows[0].track_id, 1000);
    assert!(
        store
            .move_playlist_entries(playlist, &[page.rows[0].id], None, false)
            .unwrap()
    );
    assert_eq!(
        store
            .playlist_entries_page(playlist, None, 500, 1)
            .unwrap()
            .rows[0]
            .id,
        page.rows[0].id
    );
}

#[test]
fn collection_transfer_failures_preserve_queue_and_playlist_positions() {
    use rivu_protocol::model::{
        AlbumKey, CollectionDestination, CollectionInsertion, CollectionSource,
    };
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch("INSERT INTO tracks(id,path,raw_album) VALUES(1,'/album/one.wav','Album'),(2,'/album/two.wav','Album'),(3,'/other/three.wav','Other');").unwrap();
    store.enqueue_tracks(&[3, 3]).unwrap();
    store
        .set_queue_current(Some(store.queue_ids().unwrap()[1]))
        .unwrap();
    let playlist = store.create_playlist_with_tracks("target", &[3]).unwrap();
    let source = CollectionSource::Album {
        key: AlbumKey {
            directory: "/album".into(),
            title: "Album".into(),
        },
        revision: 0,
    };
    let queue = store.queue_page(None, 0, 20).unwrap();
    let queue_state = store.queue_state().unwrap();
    let entries = store.playlist_entries_page(playlist, None, 0, 20).unwrap();
    let playlist_revision = 0;
    // Abort on a later row, after the destination gap has already been opened.
    store.conn.execute_batch("CREATE TEMP TRIGGER reject_collection_queue BEFORE INSERT ON playlist_entries WHEN new.playlist_id=0 AND new.track_id=2 BEGIN SELECT RAISE(ABORT,'rejected member'); END; CREATE TEMP TRIGGER reject_collection_playlist BEFORE INSERT ON playlist_entries WHEN new.playlist_id>0 AND new.track_id=2 BEGIN SELECT RAISE(ABORT,'rejected member'); END;").unwrap();
    for destination in [
        CollectionDestination::Queue {
            insertion: Some(CollectionInsertion {
                index: 1,
                revision: queue_state.revision,
            }),
        },
        CollectionDestination::Playlist {
            playlist_id: playlist,
            revision: playlist_revision,
            index: Some(0),
        },
    ] {
        assert!(store.transfer_collection(&source, destination).is_err());
        assert_eq!(store.queue_page(None, 0, 20).unwrap(), queue);
        assert_eq!(store.queue_state().unwrap(), queue_state);
        assert_eq!(
            store.playlist_entries_page(playlist, None, 0, 20).unwrap(),
            entries
        );
    }
    store
        .conn
        .execute_batch(
            "DROP TRIGGER reject_collection_queue; DROP TRIGGER reject_collection_playlist;",
        )
        .unwrap();
    assert_eq!(
        store
            .transfer_collection(&source, CollectionDestination::Queue { insertion: None },)
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .transfer_collection(
                &source,
                CollectionDestination::Playlist {
                    playlist_id: playlist,
                    revision: playlist_revision,
                    index: Some(0)
                },
            )
            .unwrap(),
        2
    );
    assert_eq!(store.playlist_track_ids(playlist, 20).unwrap(), [1, 2, 3]);
}

#[test]
fn empty_album_transfers_fail_without_changing_destinations() {
    use rivu_protocol::model::{AlbumKey, CollectionDestination, CollectionSource};
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute("INSERT INTO tracks(id,path) VALUES(1,'/other/one.wav')", [])
        .unwrap();
    store.enqueue_tracks(&[1]).unwrap();
    let playlist = store.create_playlist_with_tracks("target", &[1]).unwrap();
    let source = CollectionSource::Album {
        key: AlbumKey {
            directory: "/absent".into(),
            title: "Absent".into(),
        },
        revision: 0,
    };
    let queue = store.queue_page(None, 0, 10).unwrap();
    let entries = store.playlist_entries_page(playlist, None, 0, 10).unwrap();
    for destination in [
        CollectionDestination::Queue { insertion: None },
        CollectionDestination::Playlist {
            playlist_id: playlist,
            revision: 0,
            index: Some(0),
        },
    ] {
        let error = store.transfer_collection(&source, destination).unwrap_err();
        assert!(error.to_string().contains("Album is empty"));
        assert_eq!(store.queue_page(None, 0, 10).unwrap(), queue);
        assert_eq!(
            store.playlist_entries_page(playlist, None, 0, 10).unwrap(),
            entries
        );
    }
}

#[test]
fn empty_playlist_transfers_still_validate_destination_and_index() {
    use rivu_protocol::model::{CollectionDestination, CollectionInsertion, CollectionSource};
    let store = Store::open(Path::new(":memory:")).unwrap();
    let source_id = store.create_playlist("empty").unwrap();
    let target = store.create_playlist("target").unwrap();
    let source = CollectionSource::Playlist {
        playlist_id: source_id,
        revision: 0,
    };
    for destination in [
        CollectionDestination::Queue { insertion: None },
        CollectionDestination::Queue {
            insertion: Some(CollectionInsertion {
                index: 0,
                revision: 0,
            }),
        },
        CollectionDestination::Playlist {
            playlist_id: target,
            revision: 0,
            index: None,
        },
        CollectionDestination::Playlist {
            playlist_id: target,
            revision: 0,
            index: Some(0),
        },
    ] {
        assert_eq!(store.transfer_collection(&source, destination).unwrap(), 0);
    }
    for destination in [
        CollectionDestination::Queue {
            insertion: Some(CollectionInsertion {
                index: 1,
                revision: 0,
            }),
        },
        CollectionDestination::Playlist {
            playlist_id: target,
            revision: 0,
            index: Some(1),
        },
        CollectionDestination::Playlist {
            playlist_id: target + 1,
            revision: 0,
            index: None,
        },
        CollectionDestination::Playlist {
            playlist_id: target + 1,
            revision: 0,
            index: Some(0),
        },
        CollectionDestination::Playlist {
            playlist_id: 0,
            revision: 0,
            index: None,
        },
    ] {
        assert!(store.transfer_collection(&source, destination).is_err());
    }
    assert_eq!(store.queue_state().unwrap().total, 0);
    assert!(store.playlist_track_ids(target, 10).unwrap().is_empty());
    assert_eq!(
        store
            .transfer_collection(&source, CollectionDestination::Queue { insertion: None })
            .unwrap(),
        0
    );
}

#[test]
fn self_collection_transfers_preserve_staged_order_and_duplicate_occurrences() {
    use rivu_protocol::model::{CollectionDestination, CollectionSource};
    for (index, expected) in [
        (None, vec![2, 1, 2, 2, 1, 2]),
        (Some(1), vec![2, 2, 1, 2, 1, 2]),
    ] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .conn
            .execute_batch("INSERT INTO tracks(id,path) VALUES(1,'/one.wav'),(2,'/two.wav');")
            .unwrap();
        let playlist = store
            .create_playlist_with_tracks("self", &[2, 1, 2])
            .unwrap();
        let source = CollectionSource::Playlist {
            playlist_id: playlist,
            revision: 0,
        };
        let destination = CollectionDestination::Playlist {
            playlist_id: playlist,
            revision: 0,
            index,
        };
        assert_eq!(store.transfer_collection(&source, destination).unwrap(), 3);
        assert_eq!(store.playlist_track_ids(playlist, 10).unwrap(), expected);
        let entries = store
            .playlist_entries_page(playlist, None, 0, 10)
            .unwrap()
            .rows;
        assert!(
            entries
                .iter()
                .enumerate()
                .all(|(index, entry)| entry.position == index)
        );
    }
}

#[test]
fn collection_transfers_reject_position_overflow_and_can_retry_after_rollback() {
    use rivu_protocol::model::{CollectionDestination, CollectionInsertion, CollectionSource};
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute_batch("INSERT INTO tracks(id,path) VALUES(1,'/one.wav');")
        .unwrap();
    let source_id = store.create_playlist_with_tracks("source", &[1]).unwrap();
    let target = store.create_playlist_with_tracks("target", &[1]).unwrap();
    store.enqueue_tracks(&[1]).unwrap();
    let source = CollectionSource::Playlist {
        playlist_id: source_id,
        revision: 0,
    };
    for scope in [0, target] {
        store
            .conn
            .execute(
                "UPDATE playlist_entries SET position=?1 WHERE playlist_id=?2",
                params![i64::MAX - 1, scope],
            )
            .unwrap();
        for explicit in [false, true] {
            let destination = if scope == 0 {
                CollectionDestination::Queue {
                    insertion: explicit.then_some(CollectionInsertion {
                        index: 0,
                        revision: 0,
                    }),
                }
            } else {
                CollectionDestination::Playlist {
                    playlist_id: scope,
                    revision: 0,
                    index: explicit.then_some(0),
                }
            };
            let error = store.transfer_collection(&source, destination).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Insertion exceeds database positions")
            );
            let positions = store
                .conn
                .prepare("SELECT position FROM playlist_entries WHERE playlist_id=?1 ORDER BY position,id")
                .unwrap()
                .query_map([scope], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(positions, [i64::MAX - 1]);
        }
        store
            .conn
            .execute(
                "UPDATE playlist_entries SET position=0 WHERE playlist_id=?1",
                [scope],
            )
            .unwrap();
        let destination = if scope == 0 {
            CollectionDestination::Queue { insertion: None }
        } else {
            CollectionDestination::Playlist {
                playlist_id: scope,
                revision: 0,
                index: None,
            }
        };
        assert_eq!(store.transfer_collection(&source, destination).unwrap(), 1);
    }
}
