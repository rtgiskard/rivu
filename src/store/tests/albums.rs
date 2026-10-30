use super::*;

#[test]
fn albums_are_directory_scoped_compilations_with_ordered_whole_queue_actions() {
    use rivu_protocol::model::{AlbumKey, MAX_QUERY_ROWS};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("albums.db")).unwrap();
    let mut records = Vec::new();
    for (path, album, artist, disc, number, title) in [
        ("/music/a/z.flac", "Shared", "One", Some(1), Some(2), "Z"),
        ("/music/a/a.flac", "Shared", "Two", Some(1), Some(1), "A"),
        ("/music/a/n.flac", "Shared", "", None, Some(1), "N"),
        ("/music/b/a.flac", "Shared", "One", Some(1), Some(1), "B"),
        ("/music/c/u.flac", "", "", None, None, "Unknown"),
        ("/music/d/u.flac", "", "", None, None, "Unknown"),
    ] {
        let mut record = rec(path, path, title, 10.0);
        record.media.album = album.into();
        record.media.artist = artist.into();
        record.media.disc_number = disc;
        record.media.track_number = number;
        records.push(record);
    }
    for number in 0..MAX_QUERY_ROWS + 3 {
        let path = format!("/music/large/{number}.flac");
        let mut record = rec(&path, &path, &format!("Track {number}"), 10.0);
        record.media.album = "Large".into();
        record.media.track_number = Some(number as u32 + 1);
        records.push(record);
    }
    store.apply_scan(&scan(records)).unwrap();
    let albums = store.albums_page(None, AlbumSort::Title, 0, 24).unwrap();
    assert_eq!(albums.total, 3);
    assert_eq!(
        store
            .albums_page(None, AlbumSort::Title, 1, 1)
            .unwrap()
            .rows
            .len(),
        1
    );
    let compilation = albums
        .rows
        .iter()
        .find(|album| album.key.directory == Path::new("/music/a"))
        .unwrap();
    assert_eq!(compilation.artist, "Various artists");
    assert_eq!(compilation.track_count, 3);
    assert_eq!(compilation.representative.title, "A");
    assert_eq!(
        store
            .album_tracks_page(&compilation.key, None, 0, 24)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["A", "Z", "N"]
    );
    assert!(albums.rows.iter().all(|album| !album.key.title.is_empty()));
    assert_eq!(all_tracks(&store).len(), MAX_QUERY_ROWS + 9);
    let large = AlbumKey {
        directory: "/music/large".into(),
        title: "Large".into(),
    };
    assert_eq!(
        store
            .album_tracks_page(&large, None, MAX_QUERY_ROWS, 24)
            .unwrap()
            .rows
            .len(),
        3
    );
    store.queue_album(&compilation.key, false).unwrap();
    store.queue_album(&large, false).unwrap();
    assert_eq!(store.queue_state().unwrap().total, MAX_QUERY_ROWS + 6);
    store.queue_album(&large, true).unwrap();
    assert_eq!(store.queue_state().unwrap().total, MAX_QUERY_ROWS + 3);
    let before = store.queue_ids().unwrap();
    assert!(
        store
            .queue_album(
                &AlbumKey {
                    directory: "/absent".into(),
                    title: "Large".into()
                },
                true
            )
            .is_err()
    );
    assert_eq!(store.queue_ids().unwrap(), before);
    assert_eq!(
        store
            .queue_page(None, MAX_QUERY_ROWS, 24)
            .unwrap()
            .rows
            .len(),
        3
    );
    store
        .conn
        .execute(
            "UPDATE tracks SET track_number=NULL WHERE path='/music/a/z.flac'",
            [],
        )
        .unwrap();
    assert_eq!(
        store
            .album_tracks_page(&compilation.key, None, 0, 24)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["A", "Z", "N"]
    );
    store
        .conn
        .execute(
            "UPDATE tracks SET album_override='',artist_override='' WHERE path='/music/b/a.flac'",
            [],
        )
        .unwrap();
    let changed = store.albums_page(None, AlbumSort::Title, 0, 24).unwrap();
    assert_eq!(changed.total, 2);
    assert!(
        changed
            .rows
            .iter()
            .all(|album| album.key.directory != Path::new("/music/b"))
    );
}

#[test]
fn album_eligibility_matches_unicode_trim_without_normalizing_identity_or_dropping_tracks() {
    use rivu_protocol::model::AlbumKey;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("album-eligibility.db");
    let store = Store::open(&path).unwrap();
    // No application scalar functions are registered on this connection.
    let external = Connection::open(&path).unwrap();
    let whitespace = "\t\n\u{000b}\u{000c}\r \u{0085}\u{00a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}";
    let mut invalid = vec![String::new(), whitespace.to_owned()];
    invalid.extend(whitespace.chars().map(|ch| ch.to_string()));
    for (index, title) in invalid.iter().enumerate() {
        assert!(title.trim().is_empty());
        external
            .execute(
                "INSERT INTO tracks(path,raw_album,raw_artist) VALUES(?1,?2,'Excluded artist')",
                params![format!("/music/{index}.flac"), title],
            )
            .unwrap();
    }
    for (index, title) in ["", whitespace].iter().enumerate() {
        external.execute(
            "INSERT INTO tracks(path,raw_album,album_override,raw_artist) VALUES(?1,'Tagged',?2,'Excluded artist')",
            params![format!("/override/{index}.flac"), title],
        ).unwrap();
    }
    // These nonblank titles stay distinct, including surrounding whitespace
    // and Unicode characters that are not in Unicode White_Space.
    let valid = [
        "Album",
        " Album ",
        "\u{3000}Album\u{a0}",
        "\u{200b}",
        "\u{feff}",
        "\u{001c}",
        "\0",
    ];
    for (index, title) in valid.iter().enumerate() {
        assert!(!title.trim().is_empty());
        external
            .execute(
                "INSERT INTO tracks(path,raw_album) VALUES(?1,?2)",
                params![format!("/valid/{index}.flac"), title],
            )
            .unwrap();
    }
    let total = invalid.len() + 2 + valid.len();
    let tracks = all_tracks(&store);
    assert_eq!(tracks.len(), total);
    let playlist = store.create_playlist("All metadata survives").unwrap();
    let ids: Vec<_> = tracks.into_iter().map(|track| track.id).collect();
    store.add_playlist(playlist, &ids).unwrap();
    store
        .enqueue_tracks(&[ids[0], ids[0], *ids.last().unwrap()])
        .unwrap();
    store
        .set_queue_current(Some(store.queue_ids().unwrap()[1]))
        .unwrap();
    let queue = store.queue_page(None, 0, 24).unwrap();
    let state = store.queue_state().unwrap();
    let entries = store.playlist_entries_page(playlist, None, 0, 100).unwrap();
    assert_eq!(entries.total, total);
    for sort in [AlbumSort::Title, AlbumSort::Artist] {
        let page = store.albums_page(None, sort, 0, 100).unwrap();
        assert_eq!(page.total, valid.len());
        assert!(page.rows.iter().all(|album| album.artist.is_empty()));
        let titles: HashSet<_> = page
            .rows
            .iter()
            .map(|album| album.key.title.as_str())
            .collect();
        assert_eq!(titles, valid.into_iter().collect());
        assert_eq!(
            store
                .albums_page(Some("Excluded artist"), sort, 0, 24)
                .unwrap()
                .total,
            0
        );
        let past_end = store.albums_page(None, sort, valid.len(), 1).unwrap();
        assert_eq!(past_end.total, valid.len());
        assert!(past_end.rows.is_empty());
        for album in page.rows {
            assert_eq!(album.track_count, 1);
            assert_eq!(
                store
                    .album_tracks_page(&album.key, None, 0, 24)
                    .unwrap()
                    .total,
                1
            );
        }
    }
    for title in &invalid {
        for directory in ["/music", "/override"] {
            let key = AlbumKey {
                directory: directory.into(),
                title: title.clone(),
            };
            for query in [None, Some("Excluded")] {
                let page = store.album_tracks_page(&key, query, 0, 24).unwrap();
                assert_eq!(page.total, 0);
                assert!(page.rows.is_empty());
            }
            for replace in [false, true] {
                assert!(store.queue_album(&key, replace).is_err());
                assert_eq!(store.queue_page(None, 0, 24).unwrap(), queue);
                assert_eq!(store.queue_state().unwrap(), state);
            }
        }
    }
    assert_eq!(
        store.playlist_entries_page(playlist, None, 0, 100).unwrap(),
        entries
    );
    assert_eq!(all_tracks(&store).len(), total);
}

#[test]
fn album_eligibility_transitions_follow_overrides_external_commits_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("album-transitions.db");
    let store = Store::open(&path).unwrap();
    let external = Connection::open(&path).unwrap();
    external.execute_batch(
        "INSERT INTO tracks(id,path,raw_album,raw_artist) VALUES(1,'/music/one.flac','','Artist');",
    ).unwrap();
    let check = |title: Option<&str>, artist: &str| {
        for sort in [AlbumSort::Title, AlbumSort::Artist] {
            let page = store.albums_page(None, sort, 0, 24).unwrap();
            assert_eq!(page.total, usize::from(title.is_some()));
            assert_eq!(page.rows.len(), page.total);
            if let Some(title) = title {
                assert_eq!(page.rows[0].key.title, title);
                assert_eq!(page.rows[0].artist, artist);
                assert_eq!(page.rows[0].track_count, 1);
                assert_eq!(
                    store
                        .album_tracks_page(&page.rows[0].key, None, 0, 24)
                        .unwrap()
                        .total,
                    1
                );
            }
        }
        assert_eq!(all_tracks(&store).len(), 1);
    };
    check(None, "");
    external
        .execute("UPDATE tracks SET raw_album=' Tagged ' WHERE id=1", [])
        .unwrap();
    check(Some(" Tagged "), "Artist");
    let stale = store
        .albums_page(None, AlbumSort::Title, 0, 1)
        .unwrap()
        .rows
        .remove(0)
        .key;
    external
        .execute("UPDATE tracks SET album_override='' WHERE id=1", [])
        .unwrap();
    check(None, "");
    assert_eq!(
        store.album_tracks_page(&stale, None, 0, 24).unwrap().total,
        0
    );
    assert!(store.queue_album(&stale, true).is_err());
    external
        .execute("UPDATE tracks SET raw_album='Raw changed' WHERE id=1", [])
        .unwrap();
    check(None, "");
    external
        .execute("UPDATE tracks SET album_override=NULL WHERE id=1", [])
        .unwrap();
    check(Some("Raw changed"), "Artist");
    external
        .execute(
            "UPDATE tracks SET album_override=?1 WHERE id=1",
            ["\u{a0}\u{2003}\u{202f}\u{3000}"],
        )
        .unwrap();
    check(None, "");
    external.execute_batch("BEGIN; UPDATE tracks SET album_override='Rollback',artist_override='Rolled back' WHERE id=1;").unwrap();
    check(None, "");
    external.execute_batch("ROLLBACK;").unwrap();
    check(None, "");
    external.execute_batch("BEGIN; UPDATE tracks SET album_override=' Override ',artist_override='' WHERE id=1; COMMIT;").unwrap();
    check(Some(" Override "), "");
    let tx = store.conn.unchecked_transaction().unwrap();
    tx.execute("UPDATE tracks SET album_override='' WHERE id=1", [])
        .unwrap();
    check(None, "");
    tx.rollback().unwrap();
    check(Some(" Override "), "");
    external
        .execute(
            "UPDATE tracks SET raw_album='',album_override=NULL WHERE id=1",
            [],
        )
        .unwrap();
    check(None, "");
    external
        .execute("DELETE FROM tracks WHERE id=1", [])
        .unwrap();
    assert_eq!(
        store
            .albums_page(None, AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM album_artists", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn album_search_matches_effective_metadata_without_filtering_members() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("album-search.db")).unwrap();
    let mut records = Vec::new();
    for (path, album, artist) in [
        ("/music/a/1.flac", "Shared", "Needle"),
        ("/music/a/2.flac", "Shared", "Other"),
        ("/music/b/1.flac", "Shared", "Zulu"),
        ("/music/c/1.flac", "Alpha", "Alpha"),
        ("/music/d/1.flac", "Without artist", ""),
    ] {
        let mut record = rec(path, path, path, 10.0);
        record.media.album = album.into();
        record.media.artist = artist.into();
        records.push(record);
    }
    store.apply_scan(&scan(records)).unwrap();
    let matching = store
        .albums_page(Some("nEeDlE"), AlbumSort::Title, 0, 24)
        .unwrap();
    assert_eq!(matching.total, 1);
    let album = &matching.rows[0];
    assert_eq!(album.track_count, 2);
    assert_eq!(album.artist, "Various artists");
    assert_eq!(
        store
            .album_tracks_page(&album.key, None, 0, 24)
            .unwrap()
            .total,
        2
    );
    store.queue_album(&album.key, true).unwrap();
    assert_eq!(store.queue_state().unwrap().total, 2);
    assert_eq!(
        store
            .albums_page(Some("SHARED"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        2
    );
    for sort in [AlbumSort::Title, AlbumSort::Artist] {
        let first = store.albums_page(Some("shared"), sort, 0, 1).unwrap();
        let second = store.albums_page(Some("shared"), sort, 1, 1).unwrap();
        assert_eq!((first.total, second.total), (2, 2));
        assert_eq!(first.rows[0].key.directory, Path::new("/music/a"));
        assert_eq!(second.rows[0].key.directory, Path::new("/music/b"));
        assert_ne!(first.rows[0].key, second.rows[0].key);
        assert!(
            store
                .albums_page(Some("shared"), sort, 2, 1)
                .unwrap()
                .rows
                .is_empty()
        );
        let all = store.albums_page(None, sort, 0, 24).unwrap();
        assert_eq!(all.total, 4);
        assert_eq!(
            store.albums_page(Some(""), sort, 0, 24).unwrap().total,
            all.total
        );
        let absent = store.albums_page(Some("absent"), sort, 0, 24).unwrap();
        assert_eq!(absent.total, 0);
        assert!(absent.rows.is_empty());
        let keys: Vec<_> = all.rows.into_iter().map(|row| row.key).collect();
        let expected: Vec<PathBuf> = match sort {
            AlbumSort::Title => ["/music/c", "/music/a", "/music/b", "/music/d"],
            AlbumSort::Artist => ["/music/c", "/music/d", "/music/a", "/music/b"],
        }
        .into_iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(
            keys.iter()
                .map(|key| key.directory.clone())
                .collect::<Vec<_>>(),
            expected
        );
        for offset in 0..keys.len() {
            let page = store.albums_page(None, sort, offset, 1).unwrap();
            assert_eq!(page.total, keys.len());
            assert_eq!(page.rows[0].key, keys[offset]);
        }
        assert!(
            store
                .albums_page(None, sort, keys.len(), 1)
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(store.albums_page(None, sort, 0, 0).unwrap().rows.is_empty());
    }
    store.conn.execute("UPDATE tracks SET album_override='Effective',artist_override='Replacement' WHERE path='/music/b/1.flac'", []).unwrap();
    for query in ["effective", "replacement"] {
        let page = store
            .albums_page(Some(query), AlbumSort::Title, 0, 24)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].key.title, "Effective");
        assert_eq!(page.rows[0].artist, "Replacement");
    }
    assert_eq!(
        store
            .albums_page(Some("Zulu"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .albums_page(Some("Shared"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        1
    );
}

#[test]
fn album_consumers_preserve_root_unicode_and_direct_parent_boundaries() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let records = [
        ("/root.flac", "Root", 0),
        ("/音樂%_/雪.flac", "Second", 2),
        ("/音樂%_/é.flac", "First", 1),
        ("/音樂%_/disc/child.flac", "Child", 0),
        ("/音樂%_extra/other.flac", "Sibling", 0),
        ("/音樂%_0/other.flac", "Upper", 0),
    ]
    .into_iter()
    .map(|(path, title, number)| {
        let mut record = rec(path, path, title, 10.0);
        record.media.album = "共享·曲集".into();
        record.media.track_number = Some(number);
        record
    })
    .collect();
    let mut scanned = scan(records);
    scanned.roots = vec!["/".into()];
    store.apply_scan(&scanned).unwrap();
    let albums = store.albums_page(None, AlbumSort::Title, 0, 24).unwrap();
    assert_eq!(albums.total, 5);
    assert_eq!(albums.rows.len(), 5);
    for (directory, expected) in [
        ("/", &["Root"][..]),
        ("/音樂%_", &["First", "Second"][..]),
        ("/音樂%_/disc", &["Child"][..]),
        ("/音樂%_extra", &["Sibling"][..]),
        ("/音樂%_0", &["Upper"][..]),
    ] {
        let album = albums
            .rows
            .iter()
            .find(|album| album.key.directory == Path::new(directory))
            .unwrap();
        assert_eq!(album.key.title, "共享·曲集");
        assert_eq!(album.track_count, expected.len());
        assert_eq!(album.representative.title, expected[0]);
        let page = store.album_tracks_page(&album.key, None, 0, 24).unwrap();
        assert_eq!(page.total, expected.len());
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        let empty = store.album_tracks_page(&album.key, None, 0, 0).unwrap();
        assert_eq!(empty.total, expected.len());
        assert!(empty.rows.is_empty());
        store.queue_album(&album.key, true).unwrap();
        let queued = store.queue_page(None, 0, 24).unwrap();
        assert_eq!(queued.total, expected.len());
        assert_eq!(
            queued
                .rows
                .iter()
                .map(|row| row.track.title.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn album_representatives_keep_disc_track_title_and_id_priority_after_updates() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO tracks(id,path,raw_title,raw_album,disc_number,track_number) VALUES
            (1,'/music/a/1.flac','A','Shared',NULL,1),
            (2,'/music/a/2.flac','A','Shared',2,1),
            (3,'/music/a/3.flac','A','Shared',1,NULL),
            (4,'/music/a/4.flac','Z','Shared',1,2),
            (5,'/music/a/5.flac','A','Shared',1,2),
            (6,'/music/a/6.flac','A','Shared',1,2);",
        )
        .unwrap();
    let representative = || {
        let album = store
            .albums_page(None, AlbumSort::Title, 0, 1)
            .unwrap()
            .rows
            .remove(0);
        let members = store.album_tracks_page(&album.key, None, 0, 1).unwrap();
        assert_eq!(album.representative.id, members.rows[0].id);
        album.representative.id
    };
    assert_eq!(representative(), 5);
    for (sql, expected) in [
        ("UPDATE tracks SET title_override='' WHERE id=6", 6),
        ("UPDATE tracks SET disc_number=NULL WHERE id=6", 5),
        ("DELETE FROM tracks WHERE id=5", 4),
        ("UPDATE tracks SET track_number=NULL WHERE id=4", 3),
        ("UPDATE tracks SET disc_number=NULL WHERE id=3", 4),
        ("UPDATE tracks SET disc_number=NULL WHERE id=4", 2),
    ] {
        store.conn.execute(sql, []).unwrap();
        assert_eq!(representative(), expected);
    }
}

#[test]
fn album_summaries_follow_artist_counts_transactions_external_commits_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("album-transactions.db");
    let store = Store::open(&path).unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO tracks(id,path,raw_title,raw_album,raw_artist) VALUES
            (1,'/music/a/1.flac','A One','Shared','ÉTÉ_100%'),
            (2,'/music/a/2.flac','Two','Shared','ÉTÉ_100%'),
            (3,'/music/a/3.flac','Three','Shared','Other'),
            (4,'/music/a/4.flac','Four','Shared','');",
        )
        .unwrap();
    let snapshot = |store: &Store| {
        let page = store.albums_page(None, AlbumSort::Title, 0, 24).unwrap();
        (
            page.total,
            page.rows
                .into_iter()
                .map(|album| {
                    (
                        album.key,
                        album.track_count,
                        album.artist,
                        album.representative,
                    )
                })
                .collect::<Vec<_>>(),
        )
    };
    let before = snapshot(&store);
    assert_eq!(before.1[0].1, 4);
    assert_eq!(before.1[0].2, "Various artists");
    assert_eq!(
        store
            .albums_page(Some("été_100%"), AlbumSort::Artist, 0, 24)
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        store
            .albums_page(Some("Various artists"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .albums_page(Some("100x"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        0
    );
    let tx = store.conn.unchecked_transaction().unwrap();
    tx.execute_batch(
        "UPDATE tracks SET path='/moved.flac',album_override='' WHERE id=1;
         DELETE FROM tracks WHERE id=3;
         INSERT INTO tracks(path,raw_album,raw_artist) VALUES('/new/5.flac','New','New');",
    )
    .unwrap();
    assert_eq!(
        store
            .albums_page(None, AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        2
    );
    tx.rollback().unwrap();
    assert_eq!(snapshot(&store), before);

    // This connection has no Rust scalar functions or application invalidation.
    let external = Connection::open(&path).unwrap();
    external
        .execute_batch(
            "BEGIN;
         UPDATE tracks SET artist_override='ÉTÉ_100%' WHERE id=3;
         UPDATE tracks SET raw_artist='Hidden',artist_override='ÉTÉ_100%' WHERE id=1;
         COMMIT;",
        )
        .unwrap();
    let page = store
        .albums_page(Some("été_100%"), AlbumSort::Artist, 0, 24)
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].artist, "ÉTÉ_100%");
    assert_eq!(page.rows[0].representative.artist, "ÉTÉ_100%");
    external
        .execute("DELETE FROM tracks WHERE id=2", [])
        .unwrap();
    assert_eq!(snapshot(&store).1[0].2, "ÉTÉ_100%");
    external
        .execute("UPDATE tracks SET artist_override='' WHERE id=1", [])
        .unwrap();
    assert_eq!(snapshot(&store).1[0].2, "ÉTÉ_100%");
    external
        .execute("DELETE FROM tracks WHERE id=3", [])
        .unwrap();
    let page = store.albums_page(None, AlbumSort::Title, 0, 24).unwrap();
    assert_eq!(page.rows[0].track_count, 2);
    assert_eq!(page.rows[0].artist, "");
    assert_eq!(
        store
            .albums_page(Some("été"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        0
    );
    external.execute(
        "UPDATE tracks SET path='/moved.flac',album_override='',title_override='Moved' WHERE id=1", []
    ).unwrap();
    let page = store.albums_page(None, AlbumSort::Title, 0, 24).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].key.directory, Path::new("/music/a"));
    assert_eq!(page.rows[0].key.title, "Shared");
    assert_eq!(page.rows[0].track_count, 1);
    assert_eq!(page.rows[0].representative.id, 4);
    assert_eq!(store.track(1).unwrap().unwrap().title, "Moved");
    let before_restart = snapshot(&store);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(snapshot(&store), before_restart);
    external.execute("DELETE FROM tracks", []).unwrap();
    assert_eq!(snapshot(&store).0, 0);
}

#[test]
fn single_member_album_keeps_membership_across_artist_transitions_and_rollback() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO tracks(id,path,raw_album,raw_artist) VALUES
         (1,'/music/one.flac','Solo','');",
        )
        .unwrap();
    for artist in ["First", "Second", ""] {
        store.edit_track(1, None, Some(artist), None, None).unwrap();
        let page = store.albums_page(None, AlbumSort::Artist, 0, 24).unwrap();
        assert_eq!(page.total, 1);
        let album = &page.rows[0];
        assert_eq!(album.key.title, "Solo");
        assert_eq!(album.track_count, 1);
        assert_eq!(album.artist, artist);
        assert_eq!(album.representative.id, 1);
        assert_eq!(
            store
                .albums_page(Some("First"), AlbumSort::Title, 0, 24)
                .unwrap()
                .total,
            usize::from(artist == "First")
        );
    }
    let tx = store.conn.unchecked_transaction().unwrap();
    tx.execute(
        "UPDATE tracks SET artist_override='Transient' WHERE id=1",
        [],
    )
    .unwrap();
    assert_eq!(
        store
            .albums_page(Some("Transient"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        1
    );
    tx.rollback().unwrap();
    assert_eq!(
        store
            .albums_page(Some("Transient"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        0
    );
    let page = store.albums_page(None, AlbumSort::Title, 0, 24).unwrap();
    assert_eq!(page.rows[0].artist, "");
    assert_eq!(page.rows[0].track_count, 1);
}

#[test]
fn album_summaries_follow_scan_edits_removals_and_relocation_rollback() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut source = rec("/music/old/source.flac", "same", "Source", 10.0);
    source.media.album = "Old".into();
    source.media.artist = "Old artist".into();
    let mut target = rec("/music/new/target.flac", "same", "Target", 10.0);
    target.media.album = "Destination".into();
    target.media.artist = "New artist".into();
    let mut peer = rec("/music/new/peer.flac", "peer", "Peer", 10.0);
    peer.media.album = "Destination".into();
    peer.media.artist = "New artist".into();
    store
        .apply_scan(&scan(vec![source, target.clone(), peer.clone()]))
        .unwrap();
    let source_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
            "/music/old/source.flac",
        )))
        .unwrap()
        .unwrap();
    let target_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&target.path))
        .unwrap()
        .unwrap();
    let peer_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&peer.path))
        .unwrap()
        .unwrap();
    store
        .edit_track(
            source_id,
            None,
            Some("User artist"),
            Some("User album"),
            None,
        )
        .unwrap();
    store
        .conn
        .execute("UPDATE tracks SET missing=1 WHERE id=?1", [source_id])
        .unwrap();
    let prepared = store.prepare_relocation(source_id, target_id).unwrap();
    let snapshot = |store: &Store| {
        store
            .albums_page(None, AlbumSort::Artist, 0, 24)
            .unwrap()
            .rows
            .into_iter()
            .map(|album| {
                (
                    album.key,
                    album.track_count,
                    album.artist,
                    album.representative,
                )
            })
            .collect::<Vec<_>>()
    };
    let before = snapshot(&store);
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER reject_album_move BEFORE UPDATE OF path ON tracks
         BEGIN SELECT RAISE(ABORT,'reject relocation'); END;",
        )
        .unwrap();
    assert!(store.commit_relocation(&prepared).is_err());
    assert_eq!(snapshot(&store), before);
    store
        .conn
        .execute_batch("DROP TRIGGER reject_album_move;")
        .unwrap();
    store.commit_relocation(&prepared).unwrap();
    let albums = snapshot(&store);
    assert_eq!(albums.len(), 2);
    assert!(
        albums
            .iter()
            .all(|album| album.0.directory == Path::new("/music/new"))
    );
    assert!(albums.iter().all(|album| album.1 == 1));
    let moved = albums
        .iter()
        .find(|album| album.0.title == "User album")
        .unwrap();
    assert_eq!(moved.2, "User artist");
    assert_eq!(moved.3.id, source_id);
    assert_eq!(moved.3.path, target.path);
    // Scanned metadata moves an unoverridden member while preserving user edits.
    target.media.album = "Ignored raw album".into();
    target.media.artist = "Ignored raw artist".into();
    peer.media.album = "Rescanned album".into();
    peer.media.artist = "Rescanned artist".into();
    store.apply_scan(&scan(vec![target, peer])).unwrap();
    assert_eq!(
        store
            .albums_page(Some("Destination"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .albums_page(Some("Rescanned artist"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        store
            .albums_page(Some("User artist"), AlbumSort::Title, 0, 24)
            .unwrap()
            .total,
        1
    );
    store.remove_tracks(&[peer_id]).unwrap();
    let albums = snapshot(&store);
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].0.title, "User album");
    store.remove_tracks(&[source_id]).unwrap();
    assert!(snapshot(&store).is_empty());
}

#[test]
fn album_paging_work_is_independent_of_member_count_and_inserts_are_linear() {
    let measure =
        |total: usize| {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let counter = SqlVmSteps::new(&store.conn);
            store.conn.execute(
            "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<?1)
             INSERT INTO tracks(path,raw_title,raw_album,raw_artist)
             SELECT '/music/large/'||id||'.flac','Track '||id,'Large','ÉTÉ_100%' FROM ids",
            [total as i64]
        ).unwrap();
            let insert_steps = counter.take();
            for _ in 0..2 {
                for sort in [AlbumSort::Title, AlbumSort::Artist] {
                    for query in [None, Some("été_100%"), Some("absent")] {
                        let page = store.albums_page(query, sort, 0, 1).unwrap();
                        assert_eq!(page.total, usize::from(query != Some("absent")));
                        if let Some(album) = page.rows.first() {
                            assert_eq!(album.track_count, total);
                            assert_eq!(album.representative.title, "Track 1");
                        }
                    }
                }
            }
            (insert_steps, counter.take())
        };
    let (small_inserts, small_pages) = measure(128);
    let (large_inserts, large_pages) = measure(1024);
    assert!(
        large_inserts < small_inserts * 10,
        "inserts: {small_inserts} -> {large_inserts}"
    );
    assert!(
        large_pages < small_pages * 2,
        "pages: {small_pages} -> {large_pages}"
    );
}
