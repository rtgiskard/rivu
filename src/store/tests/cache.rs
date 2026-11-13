use super::*;
use crate::store::CountDomain;

#[test]
fn filtered_library_count_scopes_keep_optional_filters_distinct() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO tracks(path,raw_title,favorite,missing) VALUES
             ('/music/a','needle',1,0),
             ('/music/b','needle',0,1),
             ('relative','other',0,0);",
        )
        .unwrap();
    let count = |query: Option<&str>,
                 favorite: Option<bool>,
                 missing: Option<bool>,
                 prefix: Option<&str>| {
        store
            .cached_filtered_library_count(query, favorite, missing, prefix, || {
                Ok(store.conn.query_row(
                    "SELECT COUNT(*) FROM tracks
                     WHERE (?1 IS NULL OR instr(lower(raw_title),?1)>0)
                       AND (?2 IS NULL OR favorite=?2)
                       AND (?3 IS NULL OR missing=?3)
                       AND (?4 IS NULL OR (path>=?4 AND path<(rtrim(?4,'/')||'0')))",
                    rusqlite::params![query, favorite, missing, prefix],
                    |row| row.get::<_, i64>(0),
                )? as usize)
            })
            .unwrap()
    };
    assert_eq!(count(None, None, None, None), 3);
    // An empty prefix still restricts the BINARY path range; it is not no scope.
    assert_eq!(count(None, None, None, Some("")), 2);
    assert_eq!(count(Some(""), None, None, None), 3);
    assert_eq!(count(None, None, None, Some("")), 2);
    assert_eq!(count(Some("needle"), None, None, None), 2);
    assert_eq!(count(None, Some(true), None, None), 1);
    assert_eq!(count(None, Some(false), None, None), 2);
    assert_eq!(count(None, None, Some(true), None), 1);
    assert_eq!(count(None, None, Some(false), None), 2);
    assert_eq!(count(None, None, None, None), 3);
}

#[test]
fn writable_filtered_counts_refresh_external_commits_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("writable-counts.db");
    let store = Store::open(&path).unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO tracks(path,raw_title) VALUES
             ('/music/a','needle'),('/music/b','needle'),('/music/c','other');",
        )
        .unwrap();
    let count = || {
        store
            .library_page(Some("needle"), None, None, 0, 0)
            .unwrap()
            .total
    };
    assert_eq!(count(), 2);
    let external = Store::open(&path).unwrap();
    external
        .conn
        .execute(
            "UPDATE tracks SET raw_title='other' WHERE path='/music/a'",
            [],
        )
        .unwrap();
    assert_eq!(count(), 1);
    let transaction = store.conn.unchecked_transaction().unwrap();
    transaction
        .execute("UPDATE tracks SET raw_title='needle'", [])
        .unwrap();
    assert_eq!(count(), 3);
    transaction.rollback().unwrap();
    assert_eq!(count(), 1);
}

#[test]
fn bitrate_reader_fresh_payloads_preserve_owned_count_caches() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bitrate-counts.db");
    let writer = Store::open(&path).unwrap();
    writer
        .apply_scan(&scan(vec![rec(
            "/music/source.flac",
            "source",
            "Source",
            10.0,
        )]))
        .unwrap();
    let track = all_tracks(&writer).remove(0);
    let source = writer.playback_source(&track).unwrap().unwrap();
    let reader = Store::open_reader(&path).unwrap();
    assert!(
        reader
            .track(track.id)
            .unwrap()
            .unwrap()
            .bitrate_bps
            .is_none()
    );
    assert_eq!(
        reader
            .library_page(Some("Source"), None, None, 0, 0)
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        reader
            .cached_count(CountDomain::Queue, &["retained bitrate count"], || Ok(7))
            .unwrap(),
        7
    );
    assert!(writer.upgrade_bitrate(&source, 192_000, true).unwrap());
    assert_eq!(
        reader
            .library_page(Some("Source"), None, None, 0, 0)
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        reader
            .cached_count(CountDomain::Queue, &["retained bitrate count"], || panic!(
                "bitrate invalidated a scalar count"
            ))
            .unwrap(),
        7
    );
    let current = reader.track(track.id).unwrap().unwrap();
    assert_eq!(
        (current.bitrate_bps, current.bitrate_exact),
        (Some(192_000), true)
    );
}

#[test]
fn track_cache_refreshes_hits_before_lru_eviction() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a.mp3", "a", "A", 10.),
            rec("/music/b.mp3", "b", "B", 10.),
            rec("/music/c.mp3", "c", "C", 10.),
        ]))
        .unwrap();
    let ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
    store.set_track_cache_query_batch_size(1); // capacity is two tracks
    store.track(ids[0]).unwrap();
    store.track(ids[1]).unwrap();
    store.track(ids[0]).unwrap(); // refresh A's LRU age
    store
        .conn
        .execute("UPDATE tracks SET title_override='A2' WHERE id=?", [ids[0]])
        .unwrap();
    store
        .conn
        .execute("UPDATE tracks SET title_override='B2' WHERE id=?", [ids[1]])
        .unwrap();
    store.track(ids[2]).unwrap(); // B is oldest and must be evicted

    let cache = store.track_cache.borrow();
    assert_eq!(cache.entries.len(), 2);
    assert!(cache.entries.contains_key(&ids[0]));
    assert!(cache.entries.contains_key(&ids[2]));
    assert!(!cache.entries.contains_key(&ids[1]));
    drop(cache);
    assert_eq!(store.track(ids[0]).unwrap().unwrap().title, "A");
    assert_eq!(store.track(ids[1]).unwrap().unwrap().title, "B2");
    assert!(store.track_cache.borrow().entries.len() <= 2);
}

#[test]
fn track_cache_resize_shrinks_and_does_not_cache_misses() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a.mp3", "a", "A", 10.),
            rec("/music/b.mp3", "b", "B", 10.),
            rec("/music/c.mp3", "c", "C", 10.),
            rec("/music/d.mp3", "d", "D", 10.),
        ]))
        .unwrap();
    let ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
    store.set_track_cache_query_batch_size(2); // capacity is four tracks
    for id in &ids {
        store.track(*id).unwrap();
    }
    store.track(ids[0]).unwrap(); // retain A when shrinking
    store.set_track_cache_query_batch_size(1); // capacity is two tracks
    let cache = store.track_cache.borrow();
    assert_eq!(cache.capacity, 2);
    assert_eq!(cache.entries.len(), 2);
    assert!(cache.entries.contains_key(&ids[0]));
    assert!(cache.entries.contains_key(&ids[3]));
    assert!(cache.entries.len() <= cache.capacity);
    drop(cache);

    store.set_track_cache_query_batch_size(0);
    assert!(store.track_cache.borrow().entries.is_empty());
    assert!(store.track(999).unwrap().is_none());
    assert!(store.track_cache.borrow().entries.is_empty());
}

#[test]
fn track_cache_invalidates_edits_favorites_statistics_and_deletes() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 10.)]))
        .unwrap();
    let id = all_tracks(&store)[0].id;
    assert_eq!(store.track(id).unwrap().unwrap().title, "A");

    store.set_favorite(&[id], true).unwrap();
    assert!(store.track(id).unwrap().unwrap().favorite);
    store
        .edit_track(
            id,
            Some("Edited"),
            Some("Artist"),
            Some("Album"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().title, "Edited");
    store.mark_played(id, 42).unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().last_played, Some(42));
    store.increment_play_count(id).unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().play_count, 1);
    let stats = store.mark_played(id, 42).unwrap();
    assert_eq!(stats.play_count, 1);
    assert_eq!(stats.last_played, Some(42));
    assert!(store.mark_played(id + 1, 42).is_err());

    store.remove_tracks(&[id]).unwrap();
    assert!(store.track(id).unwrap().is_none());
    assert!(store.track_cache.borrow().entries.is_empty());
}

#[test]
fn track_cache_rescan_invalidation_returns_new_metadata() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 10.)]))
        .unwrap();
    let id = all_tracks(&store)[0].id;
    store.track(id).unwrap();
    store
        .edit_track(id, None, None, None, Some(DecoderHint::Native), None)
        .unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "Rescanned", 20.)]))
        .unwrap();
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.title, "Rescanned");
    assert_eq!(track.duration, Some(20.));
    assert_eq!(track.decoder_hint, DecoderHint::Native);
}

#[test]
fn failed_track_batch_rollback_exposes_no_uncommitted_values() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 10.)]))
        .unwrap();
    let id = all_tracks(&store)[0].id;
    assert!(!store.track(id).unwrap().unwrap().favorite);

    assert!(store.set_favorite(&[id, 999], true).is_err());
    let after_favorite = store.track(id).unwrap().unwrap();
    assert!(!after_favorite.favorite);

    assert!(store.remove_tracks(&[id, 999]).is_err());
    let after_delete = store.track(id).unwrap().unwrap();
    assert_eq!(after_delete.id, id);
    assert!(!after_delete.favorite);
    assert_eq!(after_delete.title, "A");
}

#[test]
fn reader_snapshots_refresh_counts_after_domain_invalidation_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reader.db");
    let writer = Store::open(&path).unwrap();
    writer
        .conn
        .execute_batch(
            "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<4)
         INSERT INTO tracks(id,path,raw_title,fingerprint)
         SELECT id,'/track-'||id,CASE WHEN id%2=0 THEN 'needle' ELSE 'other' END,
             CAST(printf('%032d',(id-1)/2) AS BLOB) FROM ids;",
        )
        .unwrap();
    let reader = Store::open_reader(&path).unwrap();
    let counts = || {
        let snapshot = reader.read_snapshot().unwrap();
        let filtered = snapshot
            .library_page(Some("needle"), None, None, 0, 0)
            .unwrap()
            .total;
        let duplicates = snapshot.duplicate_groups_page(0, 0).unwrap().total;
        snapshot.finish().unwrap();
        (filtered, duplicates)
    };
    assert_eq!(counts(), (2, 2));
    assert_eq!(counts(), (2, 2));
    writer
        .conn
        .execute("UPDATE tracks SET raw_title='changed' WHERE id=2", [])
        .unwrap();
    reader.invalidate_scalar_count_domains(&[CountDomain::FilteredLibrary]);
    assert_eq!(counts(), (1, 2));
    writer
        .conn
        .execute("UPDATE tracks SET fingerprint=NULL WHERE id=2", [])
        .unwrap();
    reader.invalidate_duplicate_group_count();
    assert_eq!(counts(), (1, 1));
    let transaction = writer.conn.unchecked_transaction().unwrap();
    transaction
        .execute("UPDATE tracks SET raw_title='transient'", [])
        .unwrap();
    assert_eq!(
        writer
            .library_page(Some("needle"), None, None, 0, 0)
            .unwrap()
            .total,
        0
    );
    transaction.rollback().unwrap();
    assert_eq!(
        writer
            .library_page(Some("needle"), None, None, 0, 0)
            .unwrap()
            .total,
        1
    );
}

#[test]
fn bitrate_commit_patches_only_cached_pair_after_commit() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec(
            "/music/cache.flac",
            "cache",
            "Cache",
            10.0,
        )]))
        .unwrap();
    store.set_track_cache_query_batch_size(2);
    let track = all_tracks(&store).remove(0);
    let source = store.playback_source(&track).unwrap().unwrap();
    assert_eq!(store.track(track.id).unwrap().unwrap(), track);
    let (before, age) = store.track_cache.borrow().entries[&track.id].clone();
    let clock = store.track_cache.borrow().clock;
    assert!(store.upgrade_bitrate(&source, 96_000, false).unwrap());
    let mut expected = before;
    expected.bitrate_bps = Some(96_000);
    assert_eq!(
        store.track_cache.borrow().entries[&track.id],
        (expected.clone(), age)
    );
    assert_eq!(store.track_cache.borrow().clock, clock);
    assert!(!store.upgrade_bitrate(&source, 128_000, false).unwrap());
    assert!(!store.upgrade_bitrate(&source, 0, true).unwrap());
    assert!(store.upgrade_bitrate(&source, u64::MAX, true).is_err());
    assert_eq!(
        store.track_cache.borrow().entries[&track.id],
        (expected.clone(), age)
    );
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER reject_bitrate BEFORE UPDATE OF bitrate_bps,bitrate_exact ON tracks
         BEGIN SELECT RAISE(ABORT,'reject bitrate'); END;",
        )
        .unwrap();
    assert!(store.upgrade_bitrate(&source, 192_000, true).is_err());
    assert_eq!(
        store.track_cache.borrow().entries[&track.id],
        (expected.clone(), age)
    );
    store
        .conn
        .execute_batch("DROP TRIGGER reject_bitrate")
        .unwrap();
    store.conn.execute_batch(
        "CREATE TABLE bitrate_commit_guard(track_id INTEGER REFERENCES tracks(id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TRIGGER reject_bitrate_commit AFTER UPDATE OF bitrate_bps,bitrate_exact ON tracks
         BEGIN INSERT INTO bitrate_commit_guard VALUES(-1); END;"
    ).unwrap();
    assert!(store.upgrade_bitrate(&source, 192_000, true).is_err());
    assert_eq!(
        store.track_cache.borrow().entries[&track.id],
        (expected.clone(), age)
    );
    assert_eq!(
        store
            .known_source(&source.path)
            .unwrap()
            .unwrap()
            .media
            .unwrap()
            .bitrate_bps,
        Some(96_000)
    );
    store
        .conn
        .execute_batch("DROP TRIGGER reject_bitrate_commit; DROP TABLE bitrate_commit_guard;")
        .unwrap();
    assert!(store.upgrade_bitrate(&source, 192_000, true).unwrap());
    expected.bitrate_bps = Some(192_000);
    expected.bitrate_exact = true;
    assert_eq!(
        store.track_cache.borrow().entries[&track.id],
        (expected, age)
    );
    assert_eq!(store.track_cache.borrow().clock, clock);
}

#[test]
fn bitrate_commit_does_not_insert_absent_cached_tracks() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec(
            "/music/uncached.flac",
            "uncached",
            "Uncached",
            10.0,
        )]))
        .unwrap();
    let track = all_tracks(&store).remove(0);
    let source = store.playback_source(&track).unwrap().unwrap();
    store.track_cache.borrow_mut().entries.clear();
    assert!(store.upgrade_bitrate(&source, 192_000, true).unwrap());
    assert!(store.track_cache.borrow().entries.is_empty());
    assert_eq!(
        store.track(track.id).unwrap().unwrap().bitrate_bps,
        Some(192_000)
    );
}

// Inspect SQLite's actual retained statements and execution counters, rather
// than asserting SQL spelling or relying on elapsed-time performance tests.
fn statement_activity(store: &Store) -> (usize, i64) {
    let mut count = 0;
    let mut runs = 0;
    // SAFETY: this test owns the idle connection; statements are inspected only
    // during this synchronous call and no handles escape it.
    unsafe {
        let connection = store.conn.handle();
        let mut statement = rusqlite::ffi::sqlite3_next_stmt(connection, std::ptr::null_mut());
        while !statement.is_null() {
            count += 1;
            runs += i64::from(rusqlite::ffi::sqlite3_stmt_status(
                statement,
                rusqlite::ffi::SQLITE_STMTSTATUS_RUN,
                0,
            ));
            statement = rusqlite::ffi::sqlite3_next_stmt(connection, statement);
        }
    }
    (count, runs)
}

#[test]
fn reader_playlist_counts_invalidate_only_the_changed_lifetime_token() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("playlist-count-tokens.db");
    let writer = Store::open(&path).unwrap();
    writer
        .apply_scan(&scan(vec![rec("/music/a.wav", "a", "A", 10.0)]))
        .unwrap();
    let track = all_tracks(&writer).remove(0).id;
    let a = writer.create_playlist_with_tracks("A", &[track]).unwrap();
    let b = writer.create_playlist_with_tracks("B", &[track]).unwrap();
    let reader = Store::open_reader(&path).unwrap();
    let metadata = std::sync::Arc::new(rivu_protocol::model::MetadataRevisions::default());
    let initial = std::collections::BTreeMap::from([(a, 11), (b, 12)]);
    reader.invalidate_playlist_count_revisions(&initial, &initial, &metadata, 0);
    let count = |id| reader.playlist_entries_page(id, None, 0, 0).unwrap().total;
    assert_eq!(count(a), 1);
    assert_eq!(count(b), 1);
    let original = std::collections::BTreeMap::from([(a, 11), (b, 12)]);
    let changed = std::collections::BTreeMap::from([(a, 11), (b, 13)]);
    writer.add_playlist(b, &[track]).unwrap();
    reader.invalidate_playlist_count_revisions(&original, &changed, &metadata, 0);
    let before = statement_activity(&reader);
    assert_eq!(count(a), 1);
    assert_eq!(
        statement_activity(&reader),
        before,
        "unrelated playlist count reran SQL"
    );
    assert_eq!(count(b), 2);
    assert_eq!(statement_activity(&reader).1, before.1 + 1);

    writer
        .conn
        .execute("DELETE FROM playlists WHERE id=?1", [a])
        .unwrap();
    let deleted = std::collections::BTreeMap::from([(b, 13)]);
    reader.invalidate_playlist_count_revisions(&changed, &deleted, &metadata, 0);
    assert!(reader.playlist_entries_page(a, None, 0, 0).is_err());
    writer
        .conn
        .execute(
            "INSERT INTO playlists(id,name,position) VALUES(?1,'Recreated',0)",
            [a],
        )
        .unwrap();
    let recreated = std::collections::BTreeMap::from([(a, 14), (b, 13)]);
    reader.invalidate_playlist_count_revisions(&deleted, &recreated, &metadata, 0);
    assert_eq!(count(a), 0);
    assert_eq!(count(b), 2);
}

#[test]
fn reader_playlist_search_counts_follow_metadata_and_fallback_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("playlist-search-counts.db");
    let writer = Store::open(&path).unwrap();
    writer
        .apply_scan(&scan(vec![rec("/music/a.wav", "a", "needle", 10.0)]))
        .unwrap();
    let track = all_tracks(&writer).remove(0).id;
    let playlist = writer.create_playlist_with_tracks("A", &[track]).unwrap();
    let reader = Store::open_reader(&path).unwrap();
    let versions = std::collections::BTreeMap::from([(playlist, 1)]);
    let mut library = rivu_protocol::model::LibrarySnapshot::default();
    reader.invalidate_playlist_count_revisions(
        &versions,
        &versions,
        &library.metadata_revisions,
        library.revision,
    );
    let count = || {
        reader
            .playlist_entries_page(playlist, Some("needle"), 0, 0)
            .unwrap()
            .total
    };
    assert_eq!(count(), 1);
    for (step, expected) in [(0, 0), (1, 1), (2, 0), (3, 1)] {
        writer
            .conn
            .execute(
                "UPDATE tracks SET raw_title=?1 WHERE id=?2",
                params![if expected == 1 { "needle" } else { "changed" }, track],
            )
            .unwrap();
        match step {
            0 => library.change_metadata(Some(&[track]), Some(&[playlist])),
            1 => library.change_metadata(None, None),
            2 => {
                let overflowing = vec![track; rivu_protocol::model::MAX_METADATA_SCOPES + 1];
                library.change_metadata(Some(&overflowing), Some(&[]));
            }
            _ => library.revision += 1,
        }
        reader.invalidate_playlist_count_revisions(
            &versions,
            &versions,
            &library.metadata_revisions,
            library.revision,
        );
        assert_eq!(count(), expected, "metadata transition {step}");
    }
}

#[test]
fn recurring_reader_sql_reuses_statements_across_filter_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("statement-cache.db");
    let writer = Store::open(&path).unwrap();
    writer
        .apply_scan(&scan(vec![rec("/music/a.wav", "a", "A", 10.0)]))
        .unwrap();
    let reader = Store::open_reader(&path).unwrap();
    reader
        .library_page(Some("filter-0"), None, None, 0, 1)
        .unwrap();
    let before = statement_activity(&reader);
    assert!(before.0 > 0);
    for index in 1..128 {
        assert_eq!(
            reader
                .library_page(Some(&format!("filter-{index}")), None, None, 0, 1)
                .unwrap()
                .total,
            0
        );
    }
    let after = statement_activity(&reader);
    assert_eq!(after.0, before.0, "filter values created new SQL shapes");
    assert!(after.1 > before.1);
    assert!(after.0 <= 64);
}

#[test]
fn scoped_read_cancellation_interrupts_sql_and_detaches_before_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-cancellation.db");
    let _writer = Store::open(&path).unwrap();
    let reader = Store::open_reader(&path).unwrap();
    let snapshot = reader.read_snapshot().unwrap();
    let checks = std::cell::Cell::new(0);
    let long_query = || {
        reader.conn.query_row(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT SUM(x) FROM n",
        [], |row| row.get::<_, i64>(0),
    )
    };
    let error = reader
        .with_read_cancellation(
            || {
                checks.set(checks.get() + 1);
                checks.get() == 3
            },
            long_query,
        )
        .unwrap_err();
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::OperationInterrupted)
    );
    assert_eq!(checks.get(), 3);
    snapshot.finish().unwrap();
    // A fresh snapshot and long SQL both succeed without the retired callback.
    reader.read_snapshot().unwrap().finish().unwrap();
    assert_eq!(long_query().unwrap(), 500000500000);
    assert_eq!(checks.get(), 3);
}

#[test]
fn scoped_read_cancellation_contains_callback_panics_and_detaches_on_unwind() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-cancellation-panic.db");
    let _writer = Store::open(&path).unwrap();
    let reader = Store::open_reader(&path).unwrap();
    let query = || {
        reader.conn.query_row(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) SELECT SUM(x) FROM n",
        [], |row| row.get::<_, i64>(0),
    )
    };
    let error = reader
        .with_read_cancellation(|| panic!("cancellation predicate panic"), query)
        .unwrap_err();
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::OperationInterrupted)
    );
    let checks = std::cell::Cell::new(0);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reader.with_read_cancellation(
                || {
                    checks.set(checks.get() + 1);
                    false
                },
                || panic!("read panic"),
            )
        }))
        .is_err()
    );
    assert_eq!(query().unwrap(), 500500);
    assert_eq!(
        checks.get(),
        0,
        "unwound read retained its cancellation callback"
    );
}
