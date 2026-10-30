use super::*;
use crate::store::CountDomain;

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
    assert!(reader.count_cache.borrow().is_some());
    assert_eq!(
        reader
            .cached_count(CountDomain::Queue, &["retained bitrate count"], || Ok(7))
            .unwrap(),
        7
    );
    assert!(writer.upgrade_bitrate(&source, 192_000, true).unwrap());
    assert!(reader.count_cache.borrow().is_some());
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
    store.set_track_cache_page_size(1); // capacity is two tracks
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
    store.set_track_cache_page_size(2); // capacity is four tracks
    for id in &ids {
        store.track(*id).unwrap();
    }
    store.track(ids[0]).unwrap(); // retain A when shrinking
    store.set_track_cache_page_size(1); // capacity is two tracks
    let cache = store.track_cache.borrow();
    assert_eq!(cache.capacity, 2);
    assert_eq!(cache.entries.len(), 2);
    assert!(cache.entries.contains_key(&ids[0]));
    assert!(cache.entries.contains_key(&ids[3]));
    assert!(cache.entries.len() <= cache.capacity);
    drop(cache);

    store.set_track_cache_page_size(0);
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
        .edit_track(id, Some("Edited"), Some("Artist"), Some("Album"), None)
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
        .edit_track(id, None, None, None, Some(DecoderHint::Native))
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
    reader.invalidate_filtered_track_count();
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
    store.set_track_cache_page_size(2);
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
