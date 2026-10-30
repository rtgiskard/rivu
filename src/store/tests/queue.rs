use super::*;

#[test]
fn persistent_queue_accepts_entire_large_library_pages_restarts_and_saves_playlist_in_sql() {
    fs::create_dir_all(".cache").unwrap();
    let directory = tempfile::tempdir_in(".cache").unwrap();
    let path = directory.path().join("library.db");
    let store = Store::open(&path).unwrap();
    store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<200000) INSERT INTO tracks(path,raw_title) SELECT '/music/'||n||'.wav','Track '||n FROM ids;").unwrap();
    store
        .enqueue_sources(&[PathBuf::from("/music")], &[], None)
        .unwrap();
    let state = store.queue_state().unwrap();
    assert_eq!(state.total, 200_000);
    assert!(serde_json::to_vec(&state).unwrap().len() < 160);
    let last = store.queue_page(None, 199_936, 256).unwrap();
    assert_eq!(last.rows.len(), 64);
    assert_eq!(last.rows[63].position, 199_999);
    assert_eq!(last.rows[63].track_id, 200_000);
    assert!(
        store
            .queue_page(None, 200_000, 256)
            .unwrap()
            .rows
            .is_empty()
    );
    let first = store
        .queue_batch(None, None, 256, QueryStamp::default())
        .unwrap();
    let second = store
        .queue_batch(None, first.next, 256, QueryStamp::default())
        .unwrap();
    assert_eq!(second.rows[0].position, 256);
    let current = last.rows[63].id;
    store.set_queue_current(Some(current)).unwrap();
    assert_eq!(store.queue_projection(123).unwrap().revision, 123);
    let (playlist, has_members) = store.create_playlist_from_queue("All").unwrap();
    assert!(has_members);
    assert_eq!(
        store
            .playlist_entries_page(playlist, None, 199_999, 1)
            .unwrap()
            .total,
        200_000
    );
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.queue_state().unwrap().current_id, Some(current));
    assert_eq!(store.queue_state().unwrap().current_position, Some(199_999));
    assert_eq!(store.queue_ids().unwrap().len(), 200_000);
    let before = store.queue_state().unwrap();
    assert!(store.enqueue_tracks(&[1, 999_999]).is_err());
    assert_eq!(store.queue_state().unwrap(), before);
    assert!(store.queue_entries(None, &vec![current; 257]).is_err());
    store
        .conn
        .execute("INSERT INTO tracks(path) VALUES('/music/later.wav')", [])
        .unwrap();
    assert_eq!(store.queue_state().unwrap().total, 200_000);
}

#[test]
fn queue_interval_deletion_preserves_survivors_positions_and_current_identity() {
    for removed in [
        vec![],
        vec![0],
        vec![4],
        vec![8],
        vec![1, 3, 6],
        vec![1, 2, 3],
        vec![6, 7, 8],
        vec![0, 1, 4, 5, 8],
        (0..9).collect(),
    ] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .conn
            .execute("INSERT INTO tracks(path) VALUES('/music/a.wav')", [])
            .unwrap();
        store.enqueue_tracks(&[1; 9]).unwrap();
        let ids = store.queue_ids().unwrap();
        store.set_queue_current(Some(ids[4])).unwrap();
        let mut requested: Vec<_> = removed.iter().rev().map(|&index| ids[index]).collect();
        if let Some(&id) = requested.first() {
            requested.push(id);
        }
        let before = store.queue_state().unwrap();
        assert!(store.remove_queue_entries(&[ids[0], 9999]).is_err());
        assert_eq!(store.queue_state().unwrap(), before);
        assert_eq!(store.queue_ids().unwrap(), ids);
        store.remove_queue_entries(&requested).unwrap();
        let expected: Vec<_> = ids
            .iter()
            .enumerate()
            .filter(|(index, _)| !removed.contains(index))
            .map(|(_, &id)| id)
            .collect();
        let page = store.queue_page(None, 0, 9).unwrap();
        assert_eq!(page.total, expected.len());
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
        let state = store.queue_state().unwrap();
        assert_eq!(
            state.current_id,
            expected.contains(&ids[4]).then_some(ids[4])
        );
        assert_eq!(
            state.current_position,
            expected.iter().position(|&id| id == ids[4])
        );
    }
}

#[test]
fn queue_dedup_compacts_original_slots_while_retaining_any_playing_occurrence() {
    let tracks = [1, 2, 1, 3, 2, 1, 4, 3];
    for current in 0..tracks.len() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .conn
            .execute_batch("INSERT INTO tracks(path) VALUES('/a'),('/b'),('/c'),('/d');")
            .unwrap();
        store.enqueue_tracks(&tracks).unwrap();
        let ids = store.queue_ids().unwrap();
        store.set_queue_current(Some(ids[current])).unwrap();
        let expected: Vec<_> = [1, 2, 3, 4]
            .into_iter()
            .map(|track| {
                if tracks[current] == track {
                    ids[current]
                } else {
                    ids[tracks.iter().position(|&id| id == track).unwrap()]
                }
            })
            .collect();
        store.deduplicate_queue().unwrap();
        let rows = store.queue_page(None, 0, 8).unwrap().rows;
        assert_eq!(
            rows.iter()
                .map(|row| (row.id, row.track_id, row.position))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .enumerate()
                .map(|(position, &id)| (id, position as i64 + 1, position))
                .collect::<Vec<_>>()
        );
        assert_eq!(store.queue_state().unwrap().current_id, Some(ids[current]));
        assert_eq!(
            store.queue_state().unwrap().current_position,
            expected.iter().position(|&id| id == ids[current])
        );
    }
}

#[test]
fn queue_dedup_uses_first_position_not_allocation_order_and_track_deletes_remove_references() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a.wav", "a", "A", 1.),
            rec("/music/b.wav", "b", "B", 1.),
        ]))
        .unwrap();
    store.enqueue_tracks(&[1, 2, 1]).unwrap();
    let ids = store.queue_ids().unwrap();
    store.reorder_queue(&[ids[2], ids[1], ids[0]]).unwrap();
    store.deduplicate_queue().unwrap();
    assert_eq!(store.queue_ids().unwrap(), [ids[2], ids[1]]);
    let (playlist, has_members) = store.create_playlist_from_queue("First order").unwrap();
    assert!(has_members);
    assert_eq!(store.playlist_track_ids(playlist, 2).unwrap(), [1, 2]);
    store.set_queue_current(Some(ids[2])).unwrap();
    let before = store.queue_state().unwrap();
    assert!(store.remove_tracks(&[1, 999]).is_err());
    assert_eq!(store.queue_state().unwrap(), before);
    store.remove_tracks(&[1, 1]).unwrap();
    assert_eq!(store.queue_ids().unwrap(), [ids[1]]);
    assert_eq!(store.queue_state().unwrap().current_id, None);
    assert_eq!(store.queue_page(None, 0, 1).unwrap().rows[0].position, 0);
    assert_eq!(store.playlist_track_ids(playlist, 2).unwrap(), [2]);
    store
        .conn
        .execute("UPDATE tracks SET missing=1 WHERE id=2", [])
        .unwrap();
    store.remove_missing_tracks().unwrap();
    assert_eq!(store.queue_state().unwrap().total, 0);
    assert_eq!(
        store
            .playlist_entries_page(playlist, None, 0, 1)
            .unwrap()
            .total,
        0
    );
}
