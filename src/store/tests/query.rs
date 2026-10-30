use super::*;

#[test]
fn projection_totals_and_rows_follow_filters_and_writer_changes() {
    use rivu_protocol::model::LibrarySort;
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute_batch(
            "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<1024)
         INSERT INTO tracks(id,path,raw_title,raw_album) SELECT id,'/outside/'||id||'.wav',
             CASE WHEN id%7=0 THEN 'needle' ELSE 'other' END,'Collection' FROM ids;
         INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 0,id,id-1 FROM tracks;
         INSERT INTO playlists(id,name) VALUES(1,'Count fixture');
         INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 1,id,id-1 FROM tracks;",
        )
        .unwrap();
    let roots = vec![PathBuf::from("/inside")];
    let album = rivu_protocol::model::AlbumKey {
        directory: "/outside".into(),
        title: "Collection".into(),
    };
    let counts: [&dyn Fn() -> usize; 6] = [
        &|| {
            store
                .other_tracks_page(&roots, Some("needle"), None, None, LibrarySort::Id, 0, 0)
                .unwrap()
                .total
        },
        &|| {
            store
                .directory_page(Path::new("/outside"), Some("needle"), None, None, 0, 0)
                .unwrap()
                .total
        },
        &|| {
            store
                .albums_page(Some("Collection"), AlbumSort::Title, 0, 0)
                .unwrap()
                .total
        },
        &|| {
            store
                .album_tracks_page(&album, Some("needle"), 0, 0)
                .unwrap()
                .total
        },
        &|| store.queue_page(Some("needle"), 0, 0).unwrap().total,
        &|| {
            store
                .playlist_entries_page(1, Some("needle"), 0, 0)
                .unwrap()
                .total
        },
    ];
    for (count, expected) in counts.into_iter().zip([146, 146, 1, 146, 146, 146]) {
        assert_eq!(count(), expected);
        assert_eq!(count(), expected);
    }
    let other = store
        .other_tracks_page(&roots, Some("needle"), None, None, LibrarySort::Id, 1, 2)
        .unwrap();
    assert_eq!(other.total, 146);
    assert_eq!(other.rows.len(), 2);
    let queue = store.queue_page(Some("needle"), 1, 2).unwrap();
    assert_eq!(queue.total, 146);
    assert_eq!(
        queue
            .rows
            .iter()
            .map(|row| row.track_id)
            .collect::<Vec<_>>(),
        [14, 21]
    );
    let playlist = store
        .playlist_entries_page(1, Some("needle"), 1, 2)
        .unwrap();
    assert_eq!(playlist.total, 146);
    assert_eq!(
        playlist
            .rows
            .iter()
            .map(|row| row.track_id)
            .collect::<Vec<_>>(),
        [14, 21]
    );
    assert_eq!(counts[0](), 146);
    assert_eq!(counts[1](), 146);
    assert_eq!(counts[3](), 146);
    assert_eq!(
        store
            .other_tracks_count(&[PathBuf::from("/outside")])
            .unwrap(),
        0
    );
    store
        .conn
        .execute("UPDATE tracks SET raw_title='needle' WHERE id=1", [])
        .unwrap();
    for index in [0, 1, 3, 4, 5] {
        assert_eq!(counts[index](), 147);
    }
    assert_eq!(store.queue_page(Some("absent"), 0, 0).unwrap().total, 0);
    // A count read inside an aborted writer transaction must not escape rollback.
    store
        .conn
        .execute_batch("BEGIN; UPDATE tracks SET raw_title='needle' WHERE id=2;")
        .unwrap();
    assert_eq!(counts[4](), 148);
    store.conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(counts[4](), 147);
}

fn filtered_traversal_steps(total: usize) -> (u64, u64) {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute(
            "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<?1)
         INSERT INTO tracks(id,path,raw_title) SELECT id,'track-'||id,
             CASE WHEN id%7=0 THEN 'needle' ELSE 'other' END FROM ids",
            [total as i64],
        )
        .unwrap();
    // Every track occurs twice in the queue; matches are sparse in both lists.
    store
        .conn
        .execute_batch(
            "INSERT INTO playlist_entries(playlist_id,track_id,position)
             SELECT 0,id,2*(id-1) FROM tracks UNION ALL SELECT 0,id,2*(id-1)+1 FROM tracks;
         INSERT INTO playlists(id,name) VALUES(1,'sparse');
         INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 1,id,id-1 FROM tracks;",
        )
        .unwrap();
    let stamp = QueryStamp::default();
    let mut queue_after = None;
    let mut queue_count = 0;
    // Count every SQL statement in the real batch methods, including any
    // future prefix COUNT regression, but exclude fixture setup and random pages.
    let counter = SqlVmSteps::new(&store.conn);
    loop {
        let batch = store
            .queue_batch(Some("needle"), queue_after, 17, stamp)
            .unwrap();
        for row in &batch.rows {
            assert_eq!(row.offset, queue_count);
            assert_eq!(row.track_id % 7, 0);
            assert_eq!(
                row.position,
                2 * (row.track_id as usize - 1) + queue_count % 2
            );
            queue_count += 1;
        }
        queue_after = batch.next;
        if let Some(next) = queue_after {
            assert_eq!(next.offset, queue_count);
            assert_eq!(next.cursor.id, batch.rows.last().unwrap().id);
        } else {
            break;
        }
    }
    assert_eq!(queue_count, 2 * (total / 7));
    let queue_steps = counter.take();
    let mut entry_after = None;
    let mut entry_count = 0;
    loop {
        let batch = store
            .playlist_entry_batch(1, Some("needle"), entry_after, 17, stamp)
            .unwrap();
        for row in &batch.rows {
            assert_eq!(row.offset, entry_count);
            assert_eq!(row.position, (entry_count + 1) * 7 - 1);
            entry_count += 1;
        }
        entry_after = batch.next;
        if let Some(next) = entry_after {
            assert_eq!(next.offset, entry_count);
            assert_eq!(next.cursor.id, batch.rows.last().unwrap().id);
        } else {
            break;
        }
    }
    assert_eq!(entry_count, total / 7);
    let entry_steps = counter.take();
    drop(counter);
    // Random jumps retain their independent count/OFFSET contract.
    let jump = (total / 7) / 2;
    let queue_page = store.queue_page(Some("needle"), jump, 1).unwrap();
    assert_eq!(queue_page.total, queue_count);
    assert_eq!(queue_page.rows[0].offset, jump);
    assert_eq!(queue_page.rows[0].track_id as usize, (jump / 2 + 1) * 7);
    let entry_page = store
        .playlist_entries_page(1, Some("needle"), jump, 1)
        .unwrap();
    assert_eq!(entry_page.total, entry_count);
    assert_eq!(entry_page.rows[0].offset, jump);
    assert_eq!(entry_page.rows[0].position, (jump + 1) * 7 - 1);
    (queue_steps, entry_steps)
}

#[test]
fn filtered_continuation_sql_vm_steps_scale_linearly() {
    let small = filtered_traversal_steps(4096);
    let large = filtered_traversal_steps(8192);
    eprintln!("filtered traversal VM steps (queue, playlist): 4096={small:?}, 8192={large:?}");
    assert!(small.0 > 0 && small.1 > 0);
    // Doubling the sequence must not approach the fourfold prefix-rescan cost.
    assert!(large.0 < small.0 * 3, "queue: {small:?} -> {large:?}");
    assert!(large.1 < small.1 * 3, "playlist: {small:?} -> {large:?}");
}

#[test]
fn filtered_occurrence_pages_keep_ids_positions_and_match_offsets() {
    let (_directory, store) = search_store();
    let (playlist, _) = store.create_playlist_from_queue("Needle Mix").unwrap();
    let original = store.queue_page(None, 0, 20).unwrap();
    assert_eq!(original.total, 10);
    let positions = [1, 3, 4, 5, 7, 9];
    let mut queue_matches = Vec::new();
    for offset in [0, 2, 4] {
        let queue = store.queue_page(Some("NEEDLE"), offset, 2).unwrap();
        assert_eq!(queue.total, 6);
        for (index, queued) in queue.rows.iter().enumerate() {
            let matched = offset + index;
            assert_eq!(
                (queued.position, queued.offset),
                (positions[matched], matched)
            );
            assert_eq!(queued.id, original.rows[positions[matched]].id);
        }
        queue_matches.extend(queue.rows);
    }
    assert_ne!(queue_matches[0].id, queue_matches[2].id);
    assert_eq!(queue_matches[0].track_id, queue_matches[2].track_id);
    let mut entry_matches = Vec::new();
    for offset in [0, 2, 4] {
        let entries = store
            .playlist_entries_page(playlist, Some("needle"), offset, 2)
            .unwrap();
        assert_eq!(entries.total, 6);
        for (index, entry) in entries.rows.iter().enumerate() {
            assert_eq!(
                (entry.position, entry.offset),
                (positions[offset + index], offset + index)
            );
        }
        entry_matches.extend(entries.rows);
    }
    let mut queue_after = None;
    let mut queue_batches = Vec::new();
    for _ in 0..3 {
        let queue = store
            .queue_batch(Some("needle"), queue_after, 2, QueryStamp::default())
            .unwrap();
        queue_after = queue.next;
        if let Some(cursor) = queue_after {
            assert_eq!(cursor.cursor.position, queue.rows.last().unwrap().position);
        }
        queue_batches.extend(queue.rows);
    }
    assert!(queue_after.is_none());
    let mut entry_after = None;
    let mut entry_batches = Vec::new();
    for _ in 0..3 {
        let entries = store
            .playlist_entry_batch(
                playlist,
                Some("needle"),
                entry_after,
                2,
                QueryStamp::default(),
            )
            .unwrap();
        entry_after = entries.next;
        if let Some(cursor) = entry_after {
            assert_eq!(
                cursor.cursor.position as usize,
                entries.rows.last().unwrap().position
            );
        }
        entry_batches.extend(entries.rows);
    }
    assert!(entry_after.is_none());
    assert_eq!(queue_batches, queue_matches);
    assert_eq!(entry_batches, entry_matches);
    let hidden_queue = original.rows[0].id;
    let lookup = store
        .queue_entries(
            Some("needle"),
            &[
                queue_matches[4].id,
                hidden_queue,
                queue_matches[0].id,
                queue_matches[0].id,
            ],
        )
        .unwrap();
    assert_eq!(lookup, [queue_matches[0].clone(), queue_matches[4].clone()]);
    let original_entries = store.playlist_entries_page(playlist, None, 0, 20).unwrap();
    let hidden_entry = original_entries.rows[0].id;
    let lookup = store
        .playlist_entry_locations(
            playlist,
            Some("needle"),
            &[
                entry_matches[3].id,
                hidden_entry,
                entry_matches[0].id,
                entry_matches[0].id,
            ],
        )
        .unwrap();
    assert_eq!(
        lookup
            .iter()
            .map(|row| (row.id, row.position, row.offset, row.total))
            .collect::<Vec<_>>(),
        [
            (entry_matches[0].id, 1, 0, 6),
            (entry_matches[3].id, 5, 3, 6)
        ]
    );
    assert_eq!(store.queue_page(None, 0, 20).unwrap(), original);
    assert_eq!(
        store.playlist_entries_page(playlist, None, 0, 20).unwrap(),
        original_entries
    );
    assert!(
        store
            .queue_page(Some("absent"), 0, 2)
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(store.queue_page(Some("absent"), 0, 0).unwrap().total, 0);
    assert_eq!(
        store
            .playlist_entries_page(playlist, Some("needle"), 0, 0)
            .unwrap()
            .total,
        6
    );
    assert!(
        store
            .queue_page(Some("needle"), 6, 2)
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        store
            .playlist_entries_page(playlist, Some("needle"), 6, 2)
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(
        store.queue_entries(None, &[hidden_queue]).unwrap()[0].offset,
        0
    );
    let location = store
        .playlist_entry_locations(playlist, None, &[hidden_entry])
        .unwrap()[0];
    assert_eq!(
        (location.position, location.offset, location.total),
        (0, 0, 10)
    );
}

#[test]
fn new_list_search_matches_effective_metadata_filename_and_literal_substrings() {
    let (_directory, store) = search_store();
    let (first, _) = store.create_playlist_from_queue("Mix %_ one").unwrap();
    store.create_playlist("Other").unwrap();
    let last = store.create_playlist("mix %_ two").unwrap();
    let names = store.playlist_summary_page(Some("MIX %_"), 1, 1).unwrap();
    assert_eq!(names.total, 2);
    assert_eq!(names.rows[0].id, last);
    let batch = store
        .playlist_summary_batch(Some("mix %_"), None, 1)
        .unwrap();
    assert_eq!(batch.rows[0].id, first);
    let tail = store
        .playlist_summary_batch(Some("mix %_"), batch.next, 1)
        .unwrap();
    assert_eq!(tail.rows[0].id, last);
    assert!(tail.next.is_none());
    for query in ["%_", "\\", "needle%_\\file.wav"] {
        let page = store.queue_page(Some(query), 0, 10).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].track_id, 4);
        assert_eq!(
            store
                .playlist_entries_page(first, Some(query), 0, 10)
                .unwrap()
                .rows[0]
                .track_id,
            4
        );
    }
    assert_eq!(
        store
            .queue_page(Some("directory-only"), 0, 0)
            .unwrap()
            .total,
        0
    );
    for (query, expected) in [
        ("needle title", 3),
        ("needle artist", 1),
        ("needle album", 1),
    ] {
        assert_eq!(store.queue_page(Some(query), 0, 0).unwrap().total, expected);
    }
    store
        .edit_track(1, Some("Override"), None, None, None)
        .unwrap();
    assert_eq!(
        store.queue_page(Some("needle title"), 0, 0).unwrap().total,
        0
    );
    assert_eq!(store.queue_page(Some("OVERRIDE"), 0, 0).unwrap().total, 3);
    let key = rivu_protocol::model::AlbumKey {
        directory: "/music/directory-only".into(),
        title: "Shared".into(),
    };
    let tracks = store.album_tracks_page(&key, Some("needle"), 1, 1).unwrap();
    assert_eq!(tracks.total, 2);
    assert_eq!(tracks.rows.len(), 1);
    assert_eq!(
        store
            .album_tracks_page(&key, Some("directory-only"), 0, 0)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .album_tracks_page(&key, Some("%_"), 0, 1)
            .unwrap()
            .rows[0]
            .id,
        4
    );
    assert_eq!(store.album_tracks_page(&key, None, 0, 0).unwrap().total, 4);
    assert_eq!(
        store.queue_page(Some(""), 0, 20).unwrap(),
        store.queue_page(None, 0, 20).unwrap()
    );
}

#[test]
fn bounded_view_queries_filter_order_and_count() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/c.mp3", "c", "Charlie", 1.0),
            rec("/music/a.mp3", "a", "Alpha", 1.0),
            rec("/music/b.mp3", "b", "Bravo", 1.0),
        ]))
        .unwrap();
    let ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
    store.set_favorite(&[ids[1]], true).unwrap();
    let page = store.library_page(None, Some(true), None, 0, 1).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].id, ids[1]);
    let page = store.library_page(Some("a"), None, None, 0, 2).unwrap();
    assert_eq!(
        page.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
        vec![ids[0], ids[1]]
    );
    let count_only = store
        .library_page(Some("a"), None, None, usize::MAX, 0)
        .unwrap();
    assert_eq!(count_only.total, 3);
    assert!(count_only.rows.is_empty());

    let playlist = store.create_playlist("mix").unwrap();
    store.add_playlist(playlist, &ids).unwrap();
    assert_eq!(
        store.playlist_summary_batch(None, None, 1).unwrap().rows[0].entry_count,
        3
    );
    let entries = store.playlist_entries_page(playlist, None, 1, 1).unwrap();
    assert_eq!(entries.total, 3);
    assert_eq!(entries.rows.len(), 1);
    assert_eq!(entries.rows[0].track_id, ids[1]);
    assert_eq!(store.playlist_track_ids(playlist, 2).unwrap(), ids[..2]);
}

#[test]
fn paged_totals_follow_local_writes_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("counts.db");
    let store = Store::open(&path).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a/one.wav", "one", "Needle One", 1.0),
            rec("/music/a/two.wav", "two", "Needle Two", 1.0),
            rec("/music/b/three.wav", "three", "Other", 1.0),
        ]))
        .unwrap();
    let count = || {
        store
            .library_page(Some("needle"), None, None, 0, 0)
            .unwrap()
            .total
    };
    assert_eq!(count(), 2);
    let first = store
        .library_page(Some("needle"), None, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    store
        .edit_track(first, Some("Other"), None, None, None)
        .unwrap();
    assert_eq!(count(), 1);
    store
        .conn
        .execute(
            "UPDATE tracks SET raw_title='Needle Three' WHERE path='/music/b/three.wav'",
            [],
        )
        .unwrap();
    assert_eq!(count(), 2);
    // A count read before rollback must not leak the transaction's uncommitted total.
    let tx = store.conn.unchecked_transaction().unwrap();
    tx.execute("UPDATE tracks SET title_override='Other'", [])
        .unwrap();
    assert_eq!(count(), 0);
    tx.rollback().unwrap();
    assert_eq!(count(), 2);
    for (root, total) in [("/music/a", 1), ("/music/b", 1), ("/music", 2)] {
        let page = store
            .library_root_page(
                Path::new(root),
                Some("needle"),
                None,
                None,
                rivu_protocol::model::LibrarySort::Id,
                0,
                10,
            )
            .unwrap();
        assert_eq!(page.total, total);
        assert_eq!(page.rows.len(), total);
    }
    // A different predicate or projection must not inherit another scope's count.
    assert_eq!(
        store
            .library_page(Some("other"), None, None, 0, 0)
            .unwrap()
            .total,
        1
    );
    store.set_favorite(&[first], true).unwrap();
    assert_eq!(
        store
            .library_page(None, Some(true), None, 0, 0)
            .unwrap()
            .total,
        1
    );
    assert_eq!(count(), 2);
}

#[test]
fn directory_favorites_filter_before_grouping_and_paging() {
    use rivu_protocol::model::DirectoryRow;
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&ScanResult {
            segments: vec![
                segment("/music/cue/sheet.cue", 1, 0, Some(750)),
                segment("/music/cue/sheet.cue", 2, 750, None),
            ],
            ..scan(vec![
                rec("/music/mixed/deep/yes.wav", "yes", "Yes", 60.0),
                rec("/music/mixed/no.wav", "no", "No", 60.0),
                rec("/music/empty/deep/no.wav", "empty", "Empty", 60.0),
                rec("/music/direct.wav", "direct", "Direct", 60.0),
            ])
        })
        .unwrap();
    let ids = [
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                "/music/mixed/deep/yes.wav",
            )))
            .unwrap()
            .unwrap(),
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                "/music/direct.wav",
            )))
            .unwrap()
            .unwrap(),
        store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(
                Path::new("/music/cue/sheet.cue"),
                1,
            ))
            .unwrap()
            .unwrap(),
    ];
    store.set_favorite(&ids, true).unwrap();
    let page = store
        .directory_page(Path::new("/music"), None, Some(true), None, 0, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(
        page.rows,
        [DirectoryRow::Directory {
            path: "/music/mixed".into()
        }]
    );
    let page = store
        .directory_page(Path::new("/music"), None, Some(true), None, 1, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(matches!(&page.rows[..], [DirectoryRow::Track(row)] if row.id == ids[1]));
    let page = store
        .directory_page(Path::new("/music"), None, Some(true), None, 3, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(page.rows.is_empty());
    let page = store
        .directory_page(Path::new("/music/mixed"), None, Some(true), None, 0, 10)
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(
        page.rows,
        [DirectoryRow::Directory {
            path: "/music/mixed/deep".into()
        }]
    );
    let page = store
        .directory_page(
            Path::new("/music/mixed/deep"),
            None,
            Some(true),
            None,
            0,
            10,
        )
        .unwrap();
    assert!(matches!(&page.rows[..], [DirectoryRow::Track(row)] if row.id == ids[0]));
    let page = store
        .directory_page(Path::new("/music/empty"), None, Some(true), None, 0, 10)
        .unwrap();
    assert_eq!(page.total, 0);
    assert!(page.rows.is_empty());
    let page = store
        .directory_page(Path::new("/music"), None, Some(false), None, 0, 10)
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(
        matches!(&page.rows[0], DirectoryRow::Directory { path } if path == Path::new("/music/empty"))
    );
    assert!(
        matches!(&page.rows[1], DirectoryRow::Directory { path } if path == Path::new("/music/mixed"))
    );
    assert!(matches!(&page.rows[2], DirectoryRow::Track(row) if !row.favorite && row.id != ids[2]));
    let page = store
        .directory_page(Path::new("/music"), None, None, None, 0, 10)
        .unwrap();
    assert_eq!(page.total, 5);
    let cue_rows = page
        .rows
        .iter()
        .filter_map(|row| match row {
            DirectoryRow::Track(row) if row.id != ids[1] => Some(row.id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cue_rows.len(), 2);
    assert_ne!(cue_rows[0], cue_rows[1]);
}

#[test]
fn directory_search_preserves_matching_ancestors_and_combines_favorites() {
    use rivu_protocol::model::DirectoryRow;
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a/deep/one.wav", "one", "Needle One", 60.0),
            rec("/music/a/deep/two.wav", "two", "Needle Two", 60.0),
            rec("/music/b/deep/three.wav", "three", "Needle Three", 60.0),
            rec("/music/b/other.wav", "other", "Other", 60.0),
            rec("/music/hidden/deep/no.wav", "no", "No", 60.0),
            rec("/music/direct.wav", "direct", "Needle Direct", 60.0),
            rec("/music/a-extra/edge.wav", "edge", "Needle Edge", 60.0),
        ]))
        .unwrap();
    let favorite = store
        .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
            "/music/a/deep/one.wav",
        )))
        .unwrap()
        .unwrap();
    let other = store
        .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
            "/music/b/other.wav",
        )))
        .unwrap()
        .unwrap();
    store.set_favorite(&[favorite, other], true).unwrap();
    let first = store
        .directory_page(Path::new("/music"), Some("NEEDLE"), None, None, 0, 2)
        .unwrap();
    assert_eq!(first.total, 4);
    assert_eq!(
        first.rows,
        [
            DirectoryRow::Directory {
                path: "/music/a".into()
            },
            DirectoryRow::Directory {
                path: "/music/a-extra".into()
            },
        ]
    );
    let tail = store
        .directory_page(Path::new("/music"), Some("needle"), None, None, 2, 2)
        .unwrap();
    assert_eq!(tail.total, 4);
    assert!(
        matches!(&tail.rows[0], DirectoryRow::Directory { path } if path == Path::new("/music/b"))
    );
    assert!(matches!(&tail.rows[1], DirectoryRow::Track(row) if row.title == "Needle Direct"));
    let selected = store
        .directory_page(Path::new("/music"), Some("needle"), Some(true), None, 0, 10)
        .unwrap();
    assert_eq!(selected.total, 1);
    assert_eq!(
        selected.rows,
        [DirectoryRow::Directory {
            path: "/music/a".into()
        }]
    );
    let ancestor = store
        .directory_page(
            Path::new("/music/a/"),
            Some("needle"),
            Some(true),
            None,
            0,
            10,
        )
        .unwrap();
    assert_eq!(ancestor.total, 1);
    assert_eq!(
        ancestor.rows,
        [DirectoryRow::Directory {
            path: "/music/a/deep".into()
        }]
    );
    let leaf = store
        .directory_page(
            Path::new("/music/a/deep"),
            Some("needle"),
            Some(true),
            None,
            0,
            10,
        )
        .unwrap();
    assert_eq!(leaf.total, 1);
    assert!(matches!(&leaf.rows[..], [DirectoryRow::Track(row)] if row.id == favorite));
    let non_favorites = store
        .directory_page(
            Path::new("/music/a/deep"),
            Some("needle"),
            Some(false),
            None,
            0,
            10,
        )
        .unwrap();
    assert_eq!(non_favorites.total, 1);
    assert!(
        matches!(&non_favorites.rows[..], [DirectoryRow::Track(row)] if row.title == "Needle Two")
    );
    let root = store
        .directory_page(Path::new(""), Some("needle"), Some(true), None, 0, 10)
        .unwrap();
    assert_eq!(root.total, 1);
    assert_eq!(root.rows, [DirectoryRow::Directory { path: "/".into() }]);
    for favorite in [None, Some(true), Some(false)] {
        let unfiltered = store
            .directory_page(Path::new("/music"), None, favorite, None, 1, 10)
            .unwrap();
        let empty = store
            .directory_page(Path::new("/music"), Some(""), favorite, None, 1, 10)
            .unwrap();
        assert_eq!(empty.total, unfiltered.total);
        assert_eq!(empty.rows, unfiltered.rows);
    }
    for (query, offset, limit) in [
        ("absent", 0, 10),
        ("needle", 4, 10),
        ("needle", 0, 0),
        ("needle", usize::MAX, 10),
    ] {
        let page = store
            .directory_page(Path::new("/music"), Some(query), None, None, offset, limit)
            .unwrap();
        assert_eq!(page.total, if query == "absent" { 0 } else { 4 });
        assert!(page.rows.is_empty());
    }
}

#[test]
fn directory_search_reuses_literal_unicode_metadata_and_path_matching() {
    use rivu_protocol::model::{DirectoryRow, LibrarySort};
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/search/one.wav", "one", "Raw Title", 60.0),
            rec("/music/search/two.wav", "two", "Other", 60.0),
        ]))
        .unwrap();
    store.conn.execute(
        "UPDATE tracks SET title_override='ÉTÉ_100%', artist_override='World' WHERE path='/music/search/one.wav'",
        [],
    ).unwrap();
    store
        .conn
        .execute(
            "UPDATE tracks SET raw_artist='Été_100%' WHERE path='/music/search/two.wav'",
            [],
        )
        .unwrap();
    for query in [
        "été_100%",
        "ÉTÉ_100% world",
        "/music/search/two.wav",
        "raw title",
        "étéX100Z",
    ] {
        let directory = store
            .directory_page(Path::new("/music/search"), Some(query), None, None, 0, 10)
            .unwrap();
        let library = store
            .library_view_page(Some(query), None, None, LibrarySort::Id, 0, 10)
            .unwrap();
        assert_eq!(directory.total, library.total);
        let ids = directory
            .rows
            .iter()
            .map(|row| match row {
                DirectoryRow::Track(row) => row.id,
                DirectoryRow::Directory { .. } | DirectoryRow::OtherTracks => {
                    panic!("expected only tracks")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            library.rows.iter().map(|row| row.id).collect::<Vec<_>>()
        );
    }
    assert_eq!(
        store
            .directory_page(
                Path::new("/music/search"),
                Some("été_100%"),
                None,
                None,
                0,
                10
            )
            .unwrap()
            .total,
        2
    );
    assert_eq!(
        store
            .directory_page(
                Path::new("/music/search"),
                Some("raw title"),
                None,
                None,
                0,
                10
            )
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .directory_page(
                Path::new("/music/search"),
                Some("étéX100Z"),
                None,
                None,
                0,
                10
            )
            .unwrap()
            .total,
        0
    );
}

#[test]
fn library_root_queries_preserve_path_boundaries_overlap_and_unicode() {
    use rivu_protocol::model::{LibraryRootCount, LibrarySort};
    let store = Store::open(Path::new(":memory:")).unwrap();
    let roots = [
        "/home/a",
        "/home/a/deep",
        "/home/ab",
        "/音楽",
        "/empty",
        "/",
    ]
    .map(PathBuf::from);
    let empty = store.library_root_counts(&roots).unwrap();
    assert!(empty.iter().all(|root| root.total == 0));
    for (index, path) in [
        "/home/a/one.wav",
        "/home/a/deep/two.wav",
        "/home/ab/three.wav",
        "/音楽/夏/four.wav",
        "/音楽室/five.wav",
    ]
    .iter()
    .enumerate()
    {
        store
            .conn
            .execute(
                "INSERT INTO tracks(path,raw_title,missing) VALUES(?1,?2,?3)",
                params![path, format!("Track {index}"), index == 1],
            )
            .unwrap();
    }
    let counts = store.library_root_counts(&roots).unwrap();
    assert_eq!(
        counts,
        roots
            .iter()
            .zip([2, 1, 1, 1, 0, 5])
            .map(|(path, total)| LibraryRootCount {
                path: path.clone(),
                total
            })
            .collect::<Vec<_>>()
    );
    for (path, expected) in [
        ("/home/a", 2),
        ("/home/a/", 2),
        ("/home/a/deep/..", 2),
        ("/home/a/./deep", 1),
        ("/音楽/夏", 1),
        ("/empty", 0),
        ("/", 5),
        ("", 5),
    ] {
        let page = store
            .library_root_page(Path::new(path), None, None, None, LibrarySort::Id, 0, 10)
            .unwrap();
        assert_eq!(page.total, expected, "{path}");
        assert_eq!(page.rows.len(), expected, "{path}");
        assert_eq!(
            store.library_root_counts(&[path.into()]).unwrap()[0].total,
            expected
        );
    }
    // Query normalization is lexical, including relative roots; no directory must exist.
    let cwd = std::env::current_dir().unwrap();
    store
        .conn
        .execute(
            "INSERT INTO tracks(path,raw_title) VALUES(?1,'Relative')",
            [cwd.join("root-query-relative/song.wav").to_str().unwrap()],
        )
        .unwrap();
    assert_eq!(
        store
            .library_root_page(
                Path::new("root-query-relative/./missing/.."),
                None,
                None,
                None,
                LibrarySort::Id,
                0,
                10,
            )
            .unwrap()
            .total,
        1
    );
}

#[test]
fn library_root_pages_reuse_search_filters_sort_and_total() {
    use rivu_protocol::model::LibrarySort;
    let store = Store::open(Path::new(":memory:")).unwrap();
    for (index, (path, title, artist, album, disc, number, plays, favorite, missing)) in [
        (
            "/music/a/one.wav",
            "Needle One",
            "Artist",
            "Album",
            Some(2),
            Some(1),
            2,
            true,
            false,
        ),
        (
            "/music/a/deep/two.wav",
            "Other",
            "Needle Artist",
            "Album",
            Some(1),
            Some(2),
            5,
            false,
            true,
        ),
        (
            "/music/a/three.wav",
            "Été_100%",
            "World",
            "",
            None,
            None,
            5,
            true,
            true,
        ),
        (
            "/music/ab/four.wav",
            "Needle Outside",
            "Artist",
            "Album",
            Some(1),
            Some(1),
            9,
            true,
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        store.conn.execute(
            "INSERT INTO tracks(path,raw_title,raw_artist,raw_album,disc_number,track_number,play_count,favorite,missing) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![path, title, artist, album, disc, number, plays, favorite, missing],
        ).unwrap();
        if index == 0 {
            store
                .conn
                .execute(
                    "UPDATE tracks SET title_override='Needle Override' WHERE path=?1",
                    [path],
                )
                .unwrap();
        }
    }
    for sort in [LibrarySort::Id, LibrarySort::Album, LibrarySort::MostPlayed] {
        for query in [
            None,
            Some(""),
            Some("NEEDLE"),
            Some("Été_100%"),
            Some("étéX100Z"),
            Some("/music/a/deep"),
        ] {
            for favorite in [None, Some(true), Some(false)] {
                for missing in [None, Some(true), Some(false)] {
                    let baseline = store
                        .library_view_page(query, favorite, missing, sort, 0, 10)
                        .unwrap();
                    let expected = baseline
                        .rows
                        .into_iter()
                        .filter(|row| {
                            store
                                .track(row.id)
                                .unwrap()
                                .unwrap()
                                .path
                                .starts_with("/music/a")
                        })
                        .collect::<Vec<_>>();
                    for (offset, limit) in [(0, 10), (1, 1), (10, 1), (0, 0), (usize::MAX, 10)] {
                        let page = store
                            .library_root_page(
                                Path::new("/music/a"),
                                query,
                                favorite,
                                missing,
                                sort,
                                offset,
                                limit,
                            )
                            .unwrap();
                        assert_eq!(page.total, expected.len());
                        assert_eq!(
                            page.rows,
                            expected
                                .iter()
                                .skip(offset)
                                .take(limit)
                                .cloned()
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }
    assert_eq!(
        store.library_root_counts(&["/music/a".into()]).unwrap()[0].total,
        3
    );
    assert_eq!(
        store
            .library_root_page(
                Path::new("/music/a"),
                Some("needle"),
                Some(true),
                Some(false),
                LibrarySort::Id,
                0,
                10
            )
            .unwrap()
            .rows[0]
            .title,
        "Needle Override"
    );
    for (sort, expected) in [
        (LibrarySort::Id, [1, 2, 3]),
        (LibrarySort::Album, [3, 2, 1]),
        (LibrarySort::MostPlayed, [2, 3, 1]),
    ] {
        let page = store
            .library_root_page(Path::new("/music/a"), None, None, None, sort, 0, 10)
            .unwrap();
        assert_eq!(
            page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            expected
        );
    }
    assert_eq!(
        store
            .library_root_page(
                Path::new("/music/a"),
                Some("ÉTÉ_100%"),
                None,
                None,
                LibrarySort::Id,
                0,
                10
            )
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        store
            .library_root_page(
                Path::new("/music/a"),
                Some("étéX100Z"),
                None,
                None,
                LibrarySort::Id,
                0,
                10
            )
            .unwrap()
            .total,
        0
    );
}

#[test]
fn library_root_queries_bound_root_batches_and_page_rows() {
    use rivu_protocol::model::{LibrarySort, MAX_QUERY_ROWS};
    let store = Store::open(Path::new(":memory:")).unwrap();
    assert!(store.library_root_counts(&[]).unwrap().is_empty());
    for id in 0..MAX_QUERY_ROWS + 3 {
        store
            .conn
            .execute(
                "INSERT INTO tracks(path,raw_title) VALUES(?1,?2)",
                params![format!("/music/{id}.wav"), format!("Track {id}")],
            )
            .unwrap();
    }
    // Duplicate inputs are occurrences, not a set, and remain in request order.
    let roots = (0..MAX_QUERY_ROWS)
        .map(|index| {
            if index % 2 == 0 {
                PathBuf::from("/music")
            } else {
                PathBuf::from("/empty")
            }
        })
        .collect::<Vec<_>>();
    let counts = store.library_root_counts(&roots).unwrap();
    assert_eq!(counts.len(), MAX_QUERY_ROWS);
    for (index, count) in counts.iter().enumerate() {
        assert_eq!(count.path, roots[index]);
        assert_eq!(
            count.total,
            if index % 2 == 0 {
                MAX_QUERY_ROWS + 3
            } else {
                0
            }
        );
    }
    assert!(
        store
            .library_root_counts(&vec![PathBuf::from("/music"); MAX_QUERY_ROWS + 1])
            .is_err()
    );
    let page = store
        .library_root_page(
            Path::new("/music"),
            None,
            None,
            None,
            LibrarySort::Id,
            0,
            usize::MAX,
        )
        .unwrap();
    assert_eq!(page.total, MAX_QUERY_ROWS + 3);
    assert_eq!(page.rows.len(), MAX_QUERY_ROWS);
    let tail = store
        .library_root_page(
            Path::new("/music"),
            None,
            None,
            None,
            LibrarySort::Id,
            MAX_QUERY_ROWS,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(tail.total, MAX_QUERY_ROWS + 3);
    assert_eq!(tail.rows.len(), 3);
}

#[test]
fn directory_pages_preserve_boundaries_and_duplicate_cue_sources() {
    use rivu_protocol::model::{DirectoryRow, MAX_QUERY_ROWS};
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut records = (0..300)
        .map(|index| {
            rec(
                &format!("/music/Album/{index:03}.wav"),
                &format!("hash-{index}"),
                &format!("Track {index}"),
                60.0,
            )
        })
        .collect::<Vec<_>>();
    records.push(rec("/music/AlbumExtra/other.wav", "other", "Other", 60.0));
    store
        .apply_scan(&ScanResult {
            segments: vec![
                segment("/music/Album/sheet.cue", 1, 0, Some(750)),
                segment("/music/Album/sheet.cue", 2, 750, None),
            ],
            ..scan(records)
        })
        .unwrap();
    let root = store
        .directory_page(Path::new(""), None, None, None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(root.rows, [DirectoryRow::Directory { path: "/".into() }]);
    let music = store
        .directory_page(Path::new("/music"), None, None, None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(music.total, 4);
    assert!(
        matches!(&music.rows[0], DirectoryRow::Directory { path } if path == Path::new("/music/Album"))
    );
    let first = store
        .directory_page(Path::new("/music/Album/"), None, None, None, 0, usize::MAX)
        .unwrap();
    assert_eq!(first.total, 300);
    assert_eq!(first.rows.len(), MAX_QUERY_ROWS);
    let searched = store
        .directory_page(
            Path::new("/music/Album/"),
            Some("Track "),
            None,
            None,
            0,
            usize::MAX,
        )
        .unwrap();
    assert_eq!(searched.total, 300);
    assert_eq!(searched.rows.len(), MAX_QUERY_ROWS);
    let searched_tail = store
        .directory_page(
            Path::new("/music/Album"),
            Some("Track "),
            None,
            None,
            MAX_QUERY_ROWS,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(searched_tail.total, 300);
    assert_eq!(searched_tail.rows.len(), 44);
    let last = store
        .directory_page(
            Path::new("/music/Album"),
            None,
            None,
            None,
            MAX_QUERY_ROWS,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(last.rows.len(), 44);
    assert!(matches!(&last.rows[43], DirectoryRow::Track(row) if row.title == "Track 299"));
    let cue_sources = store
        .directory_page(Path::new("/music"), None, None, None, 2, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(cue_sources.rows.len(), 2);
    assert!(
        cue_sources
            .rows
            .iter()
            .all(|row| matches!(row, DirectoryRow::Track(_)))
    );
    let cue_matches = store
        .directory_page(
            Path::new("/music"),
            Some("Track "),
            None,
            None,
            1,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(cue_matches.total, 3);
    assert_eq!(cue_matches.rows.len(), 2);
    assert!(
        cue_matches
            .rows
            .iter()
            .all(|row| matches!(row, DirectoryRow::Track(_)))
    );
    assert_ne!(cue_matches.rows[0], cue_matches.rows[1]);
    let cue_match = store
        .directory_page(
            Path::new("/music"),
            Some("Track 2"),
            None,
            None,
            1,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(cue_match.total, 2);
    assert!(matches!(&cue_match.rows[..], [DirectoryRow::Track(row)] if row.title == "Track 2"));
    let tail_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
            "/music/Album/299.wav",
        )))
        .unwrap()
        .unwrap();
    assert_eq!(store.track(tail_id).unwrap().unwrap().title, "Track 299");
    let cue_id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(
            Path::new("/music/Album/sheet.cue"),
            2,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(store.track(cue_id).unwrap().unwrap().cue.unwrap().number, 2);
    assert!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                "/music/shared.wav"
            )))
            .unwrap()
            .is_none()
    );
}

#[test]
fn library_pages_use_literal_unicode_search_and_database_sorting() {
    use rivu_protocol::model::LibrarySort;
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a.wav", "a", "ÉTÉ_100%", 60.0),
            rec("/music/b.wav", "b", "Other", 60.0),
        ]))
        .unwrap();
    let literal = store
        .library_view_page(Some("été_100%"), None, None, LibrarySort::Id, 0, 256)
        .unwrap();
    assert_eq!(literal.total, 1);
    assert_eq!(literal.rows[0].title, "ÉTÉ_100%");
    assert_eq!(
        store
            .library_page(Some("été_100%"), None, None, 0, 256)
            .unwrap()
            .total,
        1
    );
    store
        .conn
        .execute(
            "UPDATE tracks SET raw_artist='World' WHERE path='/music/b.wav'",
            [],
        )
        .unwrap();
    assert_eq!(
        store
            .library_view_page(Some("other world"), None, None, LibrarySort::Id, 0, 256)
            .unwrap()
            .total,
        store
            .library_page(Some("other world"), None, None, 0, 256)
            .unwrap()
            .total
    );
    let id = store
        .library_page(Some("Other"), None, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    store.increment_play_count(id).unwrap();
    let ranked = store
        .library_view_page(None, None, None, LibrarySort::MostPlayed, 0, 1)
        .unwrap();
    assert_eq!(ranked.rows[0].id, id);
    assert_eq!(store.library_stats().unwrap().play_count, 1);
    assert_eq!(
        store
            .library_view_page(None, None, None, LibrarySort::Id, 2, 256)
            .unwrap()
            .rows,
        []
    );
}

#[test]
fn batches_traverse_stably_validate_bounds_and_break_tied_positions() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    for id in 1..=5 {
        store
            .conn
            .execute(
                "INSERT INTO tracks(path,raw_title) VALUES(?1,?2)",
                params![format!("/music/{id}.wav"), format!("Track {id}")],
            )
            .unwrap();
    }
    assert!(store.track_batch(None, None, None, None, 0).is_err());
    let mut ids = Vec::new();
    let mut after = None;
    loop {
        let batch = store.track_batch(None, None, None, after, 2).unwrap();
        after = batch.next;
        ids.extend(batch.rows.into_iter().map(|track| track.id));
        if after.is_none() {
            break;
        }
    }
    assert_eq!(ids, [1, 2, 3, 4, 5]);
    for id in 6..=(rivu_protocol::model::MAX_QUERY_ROWS + 3) {
        store
            .conn
            .execute(
                "INSERT INTO tracks(path,raw_title) VALUES(?1,?2)",
                params![format!("/music/{id}.wav"), format!("Track {id}")],
            )
            .unwrap();
    }
    assert!(
        store
            .track_batch(
                None,
                None,
                None,
                None,
                rivu_protocol::model::MAX_QUERY_ROWS + 1
            )
            .is_err()
    );
    let first = store
        .track_batch(None, None, None, None, rivu_protocol::model::MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(first.rows.len(), rivu_protocol::model::MAX_QUERY_ROWS);
    assert_eq!(
        first.next,
        Some(rivu_protocol::model::MAX_QUERY_ROWS as i64)
    );
    let tail = store
        .track_batch(
            None,
            None,
            None,
            first.next,
            rivu_protocol::model::MAX_QUERY_ROWS,
        )
        .unwrap();
    let expected_tail: Vec<_> = ((rivu_protocol::model::MAX_QUERY_ROWS + 1)
        ..=(rivu_protocol::model::MAX_QUERY_ROWS + 3))
        .map(|id| id as i64)
        .collect();
    assert_eq!(
        tail.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
        expected_tail
    );
    assert!(tail.next.is_none());

    let first = store.create_playlist("first").unwrap();
    let second = store.create_playlist("second").unwrap();
    let third = store.create_playlist("third").unwrap();
    store.add_playlist(first, &[1, 2, 3, 4]).unwrap();
    store.add_playlist(second, &[1]).unwrap();
    let summaries = store.playlist_summary_batch(None, None, 2).unwrap();
    assert_eq!(
        summaries.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [first, second]
    );
    assert_eq!(summaries.rows[0].entry_count, 4);
    let tail = store
        .playlist_summary_batch(None, summaries.next, 2)
        .unwrap();
    assert_eq!(
        tail.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [third]
    );
    assert!(tail.next.is_none());

    store
        .conn
        .execute(
            "UPDATE playlist_entries SET position=0 WHERE playlist_id=?1",
            [first],
        )
        .unwrap();
    let entries = store
        .playlist_entry_batch(first, None, None, 2, QueryStamp::default())
        .unwrap();
    assert_eq!(
        entries.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [1, 2]
    );
    let continuation = entries.next.unwrap();
    assert_eq!(
        (continuation.cursor.position, continuation.cursor.id),
        (0, 2)
    );
    assert_eq!(continuation.offset, 2);
    let tail_entries = store
        .playlist_entry_batch(first, None, entries.next, 2, QueryStamp::default())
        .unwrap();
    assert_eq!(
        tail_entries
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [3, 4]
    );
    assert!(tail_entries.next.is_none());
    assert!(
        store
            .playlist_entry_batch(999, None, None, 1, QueryStamp::default())
            .is_err()
    );
}

#[test]
fn library_locate_matches_tree_and_flat_projection_without_identity_fallback() {
    use rivu_protocol::model::{DirectoryRow, LibraryLocateResult, LibrarySort};
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch(
        "INSERT INTO tracks(id,path,raw_title,raw_album,play_count,favorite,missing,cue_sheet,cue_number,cue_start_frame) VALUES
         (1,'/音楽/Album/shared.wav','Z','Album',2,1,0,'/音楽/Album/album.cue',1,0),
         (2,'/音楽/Album/shared.wav','A','Album',5,0,0,'/音楽/Album/album.cue',2,75),
         (3,'/音楽/top.wav','Top','',1,0,1,NULL,NULL,NULL),
         (4,'/音楽-extra/out.wav','Outside','',0,0,0,NULL,NULL,NULL);"
    ).unwrap();
    for sort in [LibrarySort::Id, LibrarySort::Album, LibrarySort::MostPlayed] {
        let page = store
            .library_root_page(Path::new("/音楽"), None, None, None, sort, 0, 20)
            .unwrap();
        for (index, row) in page.rows.iter().enumerate() {
            let LibraryLocateResult::Located(location) = store
                .library_location(row.id, Path::new("/音楽"), false, None, None, None, sort)
                .unwrap()
            else {
                panic!("expected exact flat identity");
            };
            assert_eq!(
                (location.offset, location.total, location.child),
                (index, page.total, None)
            );
        }
    }
    let LibraryLocateResult::Located(parent) = store
        .library_location(
            2,
            Path::new("/音楽"),
            true,
            None,
            None,
            None,
            LibrarySort::Album,
        )
        .unwrap()
    else {
        panic!("expected directory ancestry");
    };
    assert_eq!(parent.child.as_deref(), Some(Path::new("/音楽/Album")));
    let LibraryLocateResult::Located(track) = store
        .library_location(
            2,
            parent.child.as_deref().unwrap(),
            true,
            None,
            None,
            None,
            LibrarySort::Album,
        )
        .unwrap()
    else {
        panic!("expected CUE entry");
    };
    let page = store
        .directory_page(Path::new("/音楽/Album"), None, None, None, track.offset, 1)
        .unwrap();
    assert!(matches!(&page.rows[..], [DirectoryRow::Track(row)] if row.id == 2));
    for (query, favorite, missing) in [
        (Some("not present"), None, None),
        (None, Some(true), None),
        (None, None, Some(true)),
    ] {
        assert_eq!(
            store
                .library_location(
                    2,
                    Path::new("/音楽"),
                    true,
                    query,
                    favorite,
                    missing,
                    LibrarySort::Album
                )
                .unwrap(),
            LibraryLocateResult::Filtered
        );
    }
    assert_eq!(
        store
            .library_location(
                4,
                Path::new("/音楽"),
                false,
                None,
                None,
                None,
                LibrarySort::Id
            )
            .unwrap(),
        LibraryLocateResult::OutsideRoot
    );
    assert_eq!(
        store
            .library_location(
                999,
                Path::new("/音楽"),
                true,
                None,
                None,
                None,
                LibrarySort::Id
            )
            .unwrap(),
        LibraryLocateResult::Unavailable
    );
}

#[test]
fn all_catalog_locate_matches_empty_root_pages_for_absolute_and_relative_paths() {
    use rivu_protocol::model::{DirectoryRow, LibraryLocateResult, LibrarySort};
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch(
        "INSERT INTO tracks(id,path,raw_title,favorite,missing,cue_sheet,cue_number,cue_start_frame) VALUES
         (1,'/music/shared.wav','Needle B',1,0,'/music/album.cue',1,0),
         (2,'/music/shared.wav','Needle A',0,1,'/music/album.cue',2,75),
         (3,'relative/one.wav','Relative',1,0,NULL,NULL,NULL),
         (4,'standalone.wav','Standalone',0,1,NULL,NULL,NULL);",
    ).unwrap();
    let root = Path::new("");
    for sort in [LibrarySort::Id, LibrarySort::Album, LibrarySort::MostPlayed] {
        for (query, favorite, missing) in [
            (None, None, None),
            (Some("needle"), None, None),
            (None, Some(true), None),
            (None, None, Some(true)),
        ] {
            let flat = store
                .library_root_page(root, query, favorite, missing, sort, 0, 20)
                .unwrap();
            let tree = store
                .directory_page(root, query, favorite, missing, 0, 20)
                .unwrap();
            for (index, row) in flat.rows.iter().enumerate() {
                let LibraryLocateResult::Located(location) = store
                    .library_location(row.id, root, false, query, favorite, missing, sort)
                    .unwrap()
                else {
                    panic!("expected exact all-catalog flat identity");
                };
                assert_eq!(
                    (location.offset, location.total, location.child),
                    (index, flat.total, None)
                );
                let LibraryLocateResult::Located(location) = store
                    .library_location(row.id, root, true, query, favorite, missing, sort)
                    .unwrap()
                else {
                    panic!("expected all-catalog tree ancestry");
                };
                assert_eq!(location.total, tree.total);
                match (&tree.rows[location.offset], location.child) {
                    (DirectoryRow::Directory { path }, Some(child)) => {
                        assert_eq!(*path, child);
                        if row.path.is_absolute() {
                            assert_eq!(child, Path::new("/"));
                        }
                    }
                    (DirectoryRow::Track(track), None) => assert_eq!(track.id, row.id),
                    _ => panic!("locate must identify the displayed tree child"),
                }
            }
        }
    }
}

#[test]
fn reader_snapshots_preserve_unicode_search() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reader.db");
    let writer = Store::open(&path).unwrap();
    writer
        .conn
        .execute(
            "INSERT INTO tracks(path,raw_title) VALUES('/unicode.wav','ÄNEEDLE')",
            [],
        )
        .unwrap();
    let reader = Store::open_reader(&path).unwrap();
    let snapshot = reader.read_snapshot().unwrap();
    let page = snapshot
        .library_page(Some("äne"), None, None, 0, 1)
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].title, "ÄNEEDLE");
    snapshot.finish().unwrap();
}
