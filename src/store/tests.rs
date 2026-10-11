use super::Store;
use crate::model::{CueSegment, MediaInfo, PlaylistEntryCursor, PlaylistEntryRow, Track};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn all_tracks(store: &Store) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut after = None;
    loop {
        let batch = store
            .track_batch(None, None, None, after, crate::model::MAX_QUERY_ROWS)
            .unwrap();
        after = batch.next;
        tracks.extend(batch.rows);
        if after.is_none() {
            return tracks;
        }
    }
}
fn playlist_entries(store: &Store, playlist_id: i64) -> Vec<PlaylistEntryRow> {
    let mut entries = Vec::new();
    let mut after = None;
    loop {
        let batch = store
            .playlist_entry_batch(playlist_id, after, crate::model::MAX_QUERY_ROWS)
            .unwrap();
        after = batch.next;
        entries.extend(batch.rows);
        if after.is_none() {
            return entries;
        }
    }
}
use crate::library::{ScanRecord, ScanResult};
fn rec(path: &str, hash: &str, title: &str, dur: f64) -> ScanRecord {
    ScanRecord {
        path: path.into(),
        cue: None,
        size: 1,
        modified_ns: 1,
        fingerprint: Some(hash.into()),
        media: MediaInfo {
            title: title.into(),
            artist: String::new(),
            album: String::new(),
            duration: Some(dur),
            codec: "test".into(),
            channels: 2,
            sample_rate: 48000,
            bitrate_bps: None,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
        },
    }
}
fn scan(records: Vec<ScanRecord>) -> ScanResult {
    ScanResult {
        roots: vec![PathBuf::from("/music")],
        records,
        errors: vec![],
        suppressed_sources: Vec::new(),
        skipped_directories: Vec::new(),
        skipped_directories_truncated: false,
        scan_incomplete: false,
    }
}
fn db(name: &str) -> (PathBuf, Store) {
    let p = std::env::temp_dir().join(format!("rivu-{name}-{}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    (p.clone(), Store::open(&p).unwrap())
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
    store.edit_track(id, "Edited", "Artist", "Album").unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().title, "Edited");
    store.mark_played(id, 42).unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().last_played, Some(42));
    store.increment_play_count(id).unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().play_count, 1);

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
        .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "Rescanned", 20.)]))
        .unwrap();
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.title, "Rescanned");
    assert_eq!(track.duration, Some(20.));
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
fn playlist_deduplicates_additions_and_preserves_first_order() {
    let (p, s) = db("playlist");
    s.apply_scan(&scan(vec![
        rec("/music/a.mp3", "a", "A", 100.),
        rec("/music/b.mp3", "b", "B", 100.),
        rec("/music/c.mp3", "c", "C", 100.),
    ]))
    .unwrap();
    let ids: Vec<_> = all_tracks(&s).iter().map(|track| track.id).collect();
    let pl = s.create_playlist("mix").unwrap();
    s.add_playlist(pl, &[ids[0], ids[1], ids[0]]).unwrap();
    let first_entry = playlist_entries(&s, pl)[0].id;
    s.add_playlist(pl, &[ids[1], ids[2], ids[2]]).unwrap();
    let entries = playlist_entries(&s, pl);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(entries[0].id, first_entry);
    s.move_playlist_entry(entries[2].id, 0).unwrap();
    s.remove_playlist_entry(first_entry).unwrap();
    drop(s);
    assert_eq!(
        playlist_entries(&Store::open(&p).unwrap(), pl)
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        vec![ids[2], ids[1]]
    );
    let _ = fs::remove_file(p);
}
#[test]
fn recent_tracks_merge_replays_without_losing_counts() {
    let (p, s) = db("recent");
    s.apply_scan(&scan(vec![
        rec("/music/a.mp3", "a", "A", 600.),
        rec("/music/b.mp3", "b", "B", 100.),
    ]))
    .unwrap();
    let tracks = all_tracks(&s);
    let (a, b) = (tracks[0].id, tracks[1].id);
    s.mark_played(a, 10).unwrap();
    s.increment_play_count(a).unwrap();
    s.mark_played(b, 20).unwrap();
    s.mark_played(a, 30).unwrap();
    s.increment_play_count(a).unwrap();
    drop(s);
    let s = Store::open(&p).unwrap();
    assert_eq!(all_tracks(&s)[0].play_count, 2);
    assert_eq!(all_tracks(&s)[1].play_count, 0);
    assert_eq!(
        s.history(10)
            .unwrap()
            .iter()
            .map(|entry| (entry.track_id, entry.played_at))
            .collect::<Vec<_>>(),
        vec![(a, 30), (b, 20)]
    );
    s.remove_tracks(&[a]).unwrap();
    assert_eq!(
        s.history(10)
            .unwrap()
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        vec![b]
    );
    let _ = fs::remove_file(p);
}
#[test]
fn moved_id_preserves_override_and_delete_does_not_reuse() {
    let (p, s) = db("identity");
    s.apply_scan(&scan(vec![rec("/music/a.mp3", "same", "Raw", 100.)]))
        .unwrap();
    let id = all_tracks(&s)[0].id;
    s.edit_track(id, "Override", "A", "B").unwrap();
    s.apply_scan(&scan(vec![rec("/music/b.mp3", "same", "Changed", 100.)]))
        .unwrap();
    assert_eq!(all_tracks(&s)[0].id, id);
    assert_eq!(all_tracks(&s)[0].title, "Override");
    s.remove_tracks(&[id]).unwrap();
    s.apply_scan(&scan(vec![rec("/music/c.mp3", "new", "New", 100.)]))
        .unwrap();
    assert_ne!(all_tracks(&s)[0].id, id);
    let _ = fs::remove_file(p);
}
#[test]
fn deleting_tracks_keeps_playlist_reordering_contiguous() {
    let (path, store) = db("compact");
    store
        .apply_scan(&scan(vec![
            rec("/music/a.wav", "a", "A", 100.0),
            rec("/music/b.wav", "b", "B", 100.0),
            rec("/music/c.wav", "c", "C", 100.0),
        ]))
        .unwrap();
    let tracks = all_tracks(&store);
    let playlist = store.create_playlist("mix").unwrap();
    store
        .add_playlist(
            playlist,
            &[
                tracks[0].id,
                tracks[1].id,
                tracks[0].id,
                tracks[2].id,
                tracks[1].id,
            ],
        )
        .unwrap();
    let before = playlist_entries(&store, playlist);
    store.remove_tracks(&[tracks[0].id]).unwrap();
    store.move_playlist_entry(before[2].id, 0).unwrap();
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![before[2].id, before[1].id]
    );
    drop(store);
    let _ = fs::remove_file(path);
}

#[test]
fn metadata_refresh_preserves_favorites_and_unknown_values() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = rec("/music/tagged.wav", "hash", "A", 10.);
    record.media.bitrate_bps = Some(1_536_000);
    record.media.bits_per_sample = Some(16);
    record.media.track_number = Some(2);
    record.media.disc_number = Some(1);
    record.media.release_date = Some("2024-03".into());
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let id = all_tracks(&store)[0].id;
    store.set_favorite(&[id, id], true).unwrap();
    store.mark_played(id, 42).unwrap();
    store.increment_play_count(id).unwrap();
    record.media.track_number = Some(3);
    record.media.bitrate_bps = None;
    store.apply_scan(&scan(vec![record])).unwrap();
    let track = all_tracks(&store).remove(0);
    assert!(track.favorite);
    assert_eq!(
        (track.track_number, track.disc_number, track.bits_per_sample),
        (Some(3), Some(1), Some(16))
    );
    assert_eq!(track.release_date.as_deref(), Some("2024-03"));
    assert_eq!(track.bitrate_bps, None);
    assert_eq!((track.play_count, track.last_played), (1, Some(42)));
    assert!(store.set_favorite(&[id, 999], false).is_err());
    assert!(all_tracks(&store)[0].favorite);
    assert_eq!(
        store.known_files().unwrap()[0]
            .media
            .as_ref()
            .unwrap()
            .track_number,
        Some(3)
    );
}

#[test]
fn maintenance_retries_after_reader_releases_and_keeps_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let store = Store::open(&path).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.wav", "hash", "A", 10.)]))
        .unwrap();
    let id = all_tracks(&store)[0].id;
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM tracks;")
        .unwrap();
    store.mark_played(id, 42).unwrap();
    store.conn.busy_timeout(std::time::Duration::ZERO).unwrap();
    assert!(store.optimize().is_err());
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key='compact_pending'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(all_tracks(&reopened)[0].last_played, Some(42));
    assert_eq!(
        reopened
            .conn
            .query_row(
                "SELECT COUNT(*) FROM schema_meta WHERE key='compact_pending'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn playlist_addition_with_missing_track_rolls_back_new_entries() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a.wav", "a", "A", 10.),
            rec("/music/b.wav", "b", "B", 10.),
        ]))
        .unwrap();
    let tracks = all_tracks(&store);
    let playlist = store.create_playlist("mix").unwrap();
    store.add_playlist(playlist, &[tracks[0].id]).unwrap();
    assert!(
        store
            .add_playlist(playlist, &[tracks[0].id, tracks[1].id, 999])
            .is_err()
    );
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        vec![tracks[0].id]
    );
}

fn segment(sheet: &str, number: u32, start: u64, end: Option<u64>) -> ScanRecord {
    let mut record = rec(
        "/music/shared.wav",
        "shared",
        &format!("Track {number}"),
        60.0,
    );
    record.cue = Some(CueSegment {
        sheet: sheet.into(),
        number,
        start_frame: start,
        end_frame: end,
    });
    record
}

#[test]
fn shared_source_segments_keep_independent_ids_overrides_stats_and_missing_state() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let whole = rec("/music/shared.wav", "shared", "Whole", 180.0);
    let first = segment("/music/a.cue", 1, 0, Some(4500));
    let second = segment("/music/a.cue", 2, 4500, Some(9000));
    let other = segment("/music/b.cue", 1, 0, Some(4500));
    store
        .apply_scan(&scan(vec![whole, first.clone(), second.clone(), other]))
        .unwrap();
    let tracks = all_tracks(&store);
    let ids: Vec<_> = tracks.iter().map(|t| t.id).collect();
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 4);
    store
        .edit_track(ids[1], "Custom", "Artist", "Album")
        .unwrap();
    store.mark_played(ids[1], 10).unwrap();
    store.increment_play_count(ids[1]).unwrap();
    let playlist = store.create_playlist("Segments").unwrap();
    store
        .add_playlist(playlist, &[ids[1], ids[2], ids[1], ids[3]])
        .unwrap();
    let mut changed = first;
    changed.path = "/music/replaced.wav".into();
    changed.media.title = "New raw title".into();
    changed.media.duration = Some(59.0);
    changed.cue.as_mut().unwrap().start_frame = 75;
    changed.cue.as_mut().unwrap().end_frame = Some(4500);
    let mut rescan = scan(vec![changed.clone(), second.clone()]);
    rescan.roots = vec!["/music/a.cue".into()];
    store.apply_scan(&rescan).unwrap();
    let after = all_tracks(&store);
    assert_eq!(after.iter().map(|t| t.id).collect::<Vec<_>>(), ids);
    assert_eq!(after[1].title, "Custom");
    assert_eq!(after[1].cue, changed.cue);
    assert_eq!(after[1].path, changed.path);
    assert_eq!(after[1].duration, Some(59.0));
    assert_eq!((after[1].play_count, after[1].last_played), (1, Some(10)));
    for index in [0, 2, 3] {
        assert_eq!(
            (after[index].play_count, after[index].last_played),
            (0, None)
        );
        assert!(!after[index].missing);
    }
    let known = store.known_files().unwrap();
    let known = known.iter().find(|r| r.track_id == ids[1]).unwrap();
    assert_eq!(known.cue, changed.cue);
    assert_eq!(known.media.as_ref().unwrap().title, "New raw title");
    rescan.records = vec![second];
    store.apply_scan(&rescan).unwrap();
    let after = all_tracks(&store);
    assert!(after[1].missing);
    for index in [0, 2, 3] {
        assert!(!after[index].missing);
    }
    assert_eq!(playlist_entries(&store, playlist).len(), 3);
    rescan.records.push(changed);
    store.apply_scan(&rescan).unwrap();
    assert!(!all_tracks(&store)[1].missing);
    store.remove_tracks(&[ids[1]]).unwrap();
    assert_eq!(
        all_tracks(&store).iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![ids[0], ids[2], ids[3]]
    );
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|e| e.track_id)
            .collect::<Vec<_>>(),
        vec![ids[2], ids[3]]
    );
    assert!(store.history(10).unwrap().is_empty());
}

#[test]
fn directory_suppression_preserves_whole_source_but_missing_sheets_mark_segments() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let first = segment("/music/a.cue", 1, 0, Some(4500));
    store
        .apply_scan(&scan(vec![
            rec("/music/shared.wav", "shared", "Whole", 180.0),
            first.clone(),
        ]))
        .unwrap();
    let mut result = scan(vec![first]);
    result.suppressed_sources.push("/music/shared.wav".into());
    store.apply_scan(&result).unwrap();
    assert!(all_tracks(&store).iter().all(|t| !t.missing));
    result.records.clear();
    store.apply_scan(&result).unwrap();
    let tracks = all_tracks(&store);
    assert!(!tracks[0].missing);
    assert!(tracks[1].missing);
}

#[test]
fn depth_excluded_tracks_keep_missing_state_and_are_not_relocation_candidates() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let whole = rec("/music/deep/whole.wav", "whole", "Original", 10.0);
    let cue = segment("/music/deep/album.cue", 1, 0, None);
    let already_missing = rec(
        "/music/deep/already-missing.wav",
        "already-missing",
        "Already missing",
        10.0,
    );
    store
        .apply_scan(&scan(vec![
            whole.clone(),
            cue.clone(),
            already_missing.clone(),
            rec("/music/deeper/absent.wav", "prefix", "Other branch", 10.0),
        ]))
        .unwrap();
    let whole_id = store
        .track_id_for_source(&whole.path, None)
        .unwrap()
        .unwrap();
    let cue_id = store
        .track_id_for_source(&cue.cue.as_ref().unwrap().sheet, Some(1))
        .unwrap()
        .unwrap();
    let missing_id = store
        .track_id_for_source(&already_missing.path, None)
        .unwrap()
        .unwrap();
    store.set_favorite(&[whole_id, cue_id], true).unwrap();
    store
        .apply_scan(&ScanResult {
            roots: vec![already_missing.path.clone()],
            ..ScanResult::default()
        })
        .unwrap();
    let copy = rec("/music/copy.wav", "whole", "New copy", 10.0);
    let mut partial = scan(vec![copy.clone()]);
    partial
        .skipped_directories
        .push(crate::library::SkippedDirectory {
            root_index: 0,
            path: "/music/deep".into(),
        });
    store.apply_scan(&partial).unwrap();
    let preserved = store.track(whole_id).unwrap().unwrap();
    assert_eq!(preserved.path, whole.path);
    assert_eq!(preserved.title, "Original");
    assert!(!preserved.missing);
    assert!(preserved.favorite);
    let preserved_cue = store.track(cue_id).unwrap().unwrap();
    assert!(!preserved_cue.missing);
    assert!(preserved_cue.favorite);
    assert_eq!(
        preserved_cue.cue.as_ref().unwrap().sheet,
        Path::new("/music/deep/album.cue")
    );
    assert!(store.track(missing_id).unwrap().unwrap().missing);
    let copy_id = store
        .track_id_for_source(&copy.path, None)
        .unwrap()
        .unwrap();
    assert_ne!(copy_id, whole_id);
    let outside_id = all_tracks(&store)
        .into_iter()
        .find(|track| track.path == Path::new("/music/deeper/absent.wav"))
        .unwrap()
        .id;
    assert!(store.track(outside_id).unwrap().unwrap().missing);
    store.apply_scan(&scan(Vec::new())).unwrap();
    assert!(store.track(whole_id).unwrap().unwrap().missing);
    assert!(store.track(cue_id).unwrap().unwrap().missing);
}

#[test]
fn explicit_nested_scans_override_only_ancestor_depth_exclusions() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let sibling = rec("/music/deep/sibling.wav", "sibling", "Sibling", 10.0);
    let nested = rec("/music/deep/nested/absent.wav", "nested", "Nested", 10.0);
    let beyond = rec(
        "/music/deep/nested/beyond/hidden.wav",
        "beyond",
        "Beyond",
        10.0,
    );
    store
        .apply_scan(&scan(vec![sibling.clone(), nested.clone(), beyond.clone()]))
        .unwrap();
    let mut partial = scan(Vec::new());
    partial.roots.push("/music/deep/nested".into());
    partial.skipped_directories = vec![
        crate::library::SkippedDirectory {
            root_index: 0,
            path: "/music/deep".into(),
        },
        crate::library::SkippedDirectory {
            root_index: 1,
            path: "/music/deep/nested/beyond".into(),
        },
    ];
    store.apply_scan(&partial).unwrap();
    for (record, missing) in [(sibling, false), (nested, true), (beyond, false)] {
        let id = all_tracks(&store)
            .into_iter()
            .find(|track| track.path == record.path)
            .unwrap()
            .id;
        assert_eq!(store.track(id).unwrap().unwrap().missing, missing);
    }
}

#[test]
fn outer_exclusions_do_not_hide_a_completed_nested_scan() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let deep = rec("/music/nested/covered/deep.wav", "deep", "Deep", 10.0);
    store.apply_scan(&scan(vec![deep.clone()])).unwrap();
    let id = store
        .track_id_for_source(&deep.path, None)
        .unwrap()
        .unwrap();
    let mut partial = scan(Vec::new());
    partial.roots.push("/music/nested".into());
    partial
        .skipped_directories
        .push(crate::library::SkippedDirectory {
            root_index: 0,
            path: "/music/nested/covered".into(),
        });
    store.apply_scan(&partial).unwrap();
    assert!(store.track(id).unwrap().unwrap().missing);
}

#[test]
fn incomplete_scan_preserves_absent_identities_and_avoids_relocation() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let old = rec("/music/old.wav", "same", "Original", 10.0);
    store.apply_scan(&scan(vec![old.clone()])).unwrap();
    let id = store.track_id_for_source(&old.path, None).unwrap().unwrap();
    store.set_favorite(&[id], true).unwrap();
    let copy = rec("/music/copy.wav", "same", "Copy", 10.0);
    let mut incomplete = scan(vec![copy.clone()]);
    incomplete.scan_incomplete = true;
    store.apply_scan(&incomplete).unwrap();
    let preserved = store.track(id).unwrap().unwrap();
    assert_eq!(preserved.path, old.path);
    assert!(preserved.favorite);
    assert!(!preserved.missing);
    assert_ne!(
        store
            .track_id_for_source(&copy.path, None)
            .unwrap()
            .unwrap(),
        id
    );
    store.apply_scan(&scan(vec![copy])).unwrap();
    assert!(store.track(id).unwrap().unwrap().missing);
}

#[test]
fn fingerprints_never_merge_full_files_or_different_cue_segments() {
    let root = std::env::temp_dir().join(format!("rivu-cue-matches-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let cases = [
        (None, Some((1, 0, Some(4500))), false),
        (Some((1, 0, Some(4500))), None, false),
        (Some((1, 0, Some(4500))), Some((2, 0, Some(4500))), false),
        (Some((1, 0, Some(4500))), Some((1, 75, Some(4500))), false),
        (Some((1, 0, Some(4500))), Some((1, 0, None)), false),
        (Some((1, 0, Some(4500))), Some((1, 0, Some(4500))), true),
    ];
    for (index, (before, after, same_id)) in cases.into_iter().enumerate() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let make_cue = |value: Option<(u32, u64, Option<u64>)>, name: &str| {
            value.map(|(number, start_frame, end_frame)| CueSegment {
                sheet: root.join(format!("{index}-{name}.cue")),
                number,
                start_frame,
                end_frame,
            })
        };
        let mut old = rec("", "same-hash", "Old", 60.0);
        old.path = root.join(format!("{index}-old.wav"));
        old.cue = make_cue(before, "old");
        let mut new = rec("", "same-hash", "New", 60.0);
        new.path = root.join(format!("{index}-new.wav"));
        new.cue = make_cue(after, "new");
        let mut result = scan(vec![old]);
        result.roots = vec![root.clone()];
        store.apply_scan(&result).unwrap();
        let old_id = all_tracks(&store)[0].id;
        result.records = vec![new];
        store.apply_scan(&result).unwrap();
        let tracks = all_tracks(&store);
        let new_track = tracks.iter().find(|t| t.title == "New").unwrap();
        assert_eq!(new_track.id == old_id, same_id, "case {index}");
        assert_eq!(tracks.len(), if same_id { 1 } else { 2 });
        assert!(!new_track.missing);
        if !same_id {
            assert!(tracks.iter().find(|t| t.id == old_id).unwrap().missing);
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cue_relocation_requires_absent_sheet_not_absent_shared_source() {
    let root = std::env::temp_dir().join(format!("rivu-cue-move-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let sheet = root.join("old.cue");
    let source = root.join("shared.wav");
    fs::write(&sheet, b"sheet").unwrap();
    fs::write(&source, b"source").unwrap();
    let mut old = segment(sheet.to_str().unwrap(), 1, 0, Some(4500));
    old.path = source.clone();
    let mut moved = old.clone();
    moved.cue.as_mut().unwrap().sheet = root.join("new.cue");
    for sheet_exists in [true, false] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut result = scan(vec![old.clone()]);
        result.roots = vec![root.clone()];
        store.apply_scan(&result).unwrap();
        let id = all_tracks(&store)[0].id;
        store.edit_track(id, "Override", "Artist", "Album").unwrap();
        if !sheet_exists {
            fs::remove_file(&sheet).unwrap();
        }
        result.records = vec![moved.clone()];
        store.apply_scan(&result).unwrap();
        let tracks = all_tracks(&store);
        let new = tracks.iter().find(|t| t.cue == moved.cue).unwrap();
        assert_eq!(new.id == id, !sheet_exists);
        if !sheet_exists {
            assert_eq!(new.title, "Override");
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ambiguous_cue_relocation_does_not_steal_an_existing_id() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            segment("/music/old-a.cue", 1, 0, Some(4500)),
            segment("/music/old-b.cue", 1, 0, Some(4500)),
        ]))
        .unwrap();
    let ids: Vec<_> = all_tracks(&store).iter().map(|t| t.id).collect();
    store
        .apply_scan(&scan(vec![segment("/music/new.cue", 1, 0, Some(4500))]))
        .unwrap();
    let tracks = all_tracks(&store);
    assert_eq!(tracks.len(), 3);
    assert!(!ids.contains(&tracks[2].id));
}

#[test]
fn invalid_cue_database_range_leaves_entire_scan_unchanged() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec(
            "/music/original.wav",
            "old",
            "Original",
            60.0,
        )]))
        .unwrap();
    let invalid = segment("/music/a.cue", 1, u64::MAX, None);
    assert!(
        store
            .apply_scan(&scan(vec![
                rec("/music/new.wav", "new", "New", 60.0),
                invalid
            ]))
            .is_err()
    );
    let tracks = all_tracks(&store);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Original");
    assert!(!tracks[0].missing);
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

    let playlist = store.create_playlist("mix").unwrap();
    store.add_playlist(playlist, &ids).unwrap();
    assert_eq!(
        store.playlist_summary_batch(None, 1).unwrap().rows[0].entry_count,
        3
    );
    let entries = store.playlist_entries_page(playlist, 1, 1).unwrap();
    assert_eq!(entries.total, 3);
    assert_eq!(entries.rows.len(), 1);
    assert_eq!(entries.rows[0].track_id, ids[1]);
    assert_eq!(store.playlist_track_ids(playlist, 2).unwrap(), ids[..2]);
}

#[test]
fn directory_favorites_filter_before_grouping_and_paging() {
    use crate::model::DirectoryRow;
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/mixed/deep/yes.wav", "yes", "Yes", 60.0),
            rec("/music/mixed/no.wav", "no", "No", 60.0),
            rec("/music/empty/deep/no.wav", "empty", "Empty", 60.0),
            rec("/music/direct.wav", "direct", "Direct", 60.0),
            segment("/music/cue/sheet.cue", 1, 0, Some(750)),
            segment("/music/cue/sheet.cue", 2, 750, None),
        ]))
        .unwrap();
    let ids = [
        store
            .track_id_for_source(Path::new("/music/mixed/deep/yes.wav"), None)
            .unwrap()
            .unwrap(),
        store
            .track_id_for_source(Path::new("/music/direct.wav"), None)
            .unwrap()
            .unwrap(),
        store
            .track_id_for_source(Path::new("/music/cue/sheet.cue"), Some(1))
            .unwrap()
            .unwrap(),
    ];
    store.set_favorite(&ids, true).unwrap();
    let page = store
        .directory_page(Path::new("/music"), Some(true), 0, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(
        page.rows,
        [DirectoryRow::Directory {
            path: "/music/mixed".into()
        }]
    );
    let page = store
        .directory_page(Path::new("/music"), Some(true), 1, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(matches!(&page.rows[..], [DirectoryRow::Track(row)] if row.id == ids[1]));
    let page = store
        .directory_page(Path::new("/music"), Some(true), 3, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(page.rows.is_empty());
    let page = store
        .directory_page(Path::new("/music/mixed"), Some(true), 0, 10)
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(
        page.rows,
        [DirectoryRow::Directory {
            path: "/music/mixed/deep".into()
        }]
    );
    let page = store
        .directory_page(Path::new("/music/mixed/deep"), Some(true), 0, 10)
        .unwrap();
    assert!(matches!(&page.rows[..], [DirectoryRow::Track(row)] if row.id == ids[0]));
    let page = store
        .directory_page(Path::new("/music/empty"), Some(true), 0, 10)
        .unwrap();
    assert_eq!(page.total, 0);
    assert!(page.rows.is_empty());
    let page = store
        .directory_page(Path::new("/music"), Some(false), 0, 10)
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
        .directory_page(Path::new("/music"), None, 0, 10)
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
fn directory_pages_preserve_boundaries_and_duplicate_cue_sources() {
    use crate::model::{DirectoryRow, MAX_QUERY_ROWS};
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
    records.push(segment("/music/Album/sheet.cue", 1, 0, Some(750)));
    records.push(segment("/music/Album/sheet.cue", 2, 750, None));
    store.apply_scan(&scan(records)).unwrap();
    let root = store
        .directory_page(Path::new(""), None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(root.rows, [DirectoryRow::Directory { path: "/".into() }]);
    let music = store
        .directory_page(Path::new("/music"), None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(music.total, 4);
    assert!(
        matches!(&music.rows[0], DirectoryRow::Directory { path } if path == Path::new("/music/Album"))
    );
    let first = store
        .directory_page(Path::new("/music/Album/"), None, 0, usize::MAX)
        .unwrap();
    assert_eq!(first.total, 300);
    assert_eq!(first.rows.len(), MAX_QUERY_ROWS);
    let last = store
        .directory_page(
            Path::new("/music/Album"),
            None,
            MAX_QUERY_ROWS,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(last.rows.len(), 44);
    assert!(matches!(&last.rows[43], DirectoryRow::Track(row) if row.title == "Track 299"));
    let cue_sources = store
        .directory_page(Path::new("/music"), None, 2, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(cue_sources.rows.len(), 2);
    assert!(
        cue_sources
            .rows
            .iter()
            .all(|row| matches!(row, DirectoryRow::Track(_)))
    );
    let tail_id = store
        .track_id_for_source(Path::new("/music/Album/299.wav"), None)
        .unwrap()
        .unwrap();
    assert_eq!(store.track(tail_id).unwrap().unwrap().title, "Track 299");
    let cue_id = store
        .track_id_for_source(Path::new("/music/Album/sheet.cue"), Some(2))
        .unwrap()
        .unwrap();
    assert_eq!(store.track(cue_id).unwrap().unwrap().cue.unwrap().number, 2);
    assert!(
        store
            .track_id_for_source(Path::new("/music/shared.wav"), None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn library_pages_use_literal_unicode_search_and_database_sorting() {
    use crate::model::LibrarySort;
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
fn directory_selections_deduplicate_atomically_without_capping_playlists() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<5000) INSERT INTO tracks(path) SELECT '/music/' || CASE WHEN n<=2500 THEN 'A/' ELSE 'B/' END || n || '.wav' FROM ids").unwrap();
    let directories = vec![
        PathBuf::from("/music/A"),
        PathBuf::from("/music/B"),
        PathBuf::from("/music/A"),
    ];
    assert!(store.source_track_ids(&directories, &[1], 4096).is_err());
    let ids = store
        .source_track_ids(&directories[..1], &[1, 1], 2500)
        .unwrap();
    assert_eq!(ids.len(), 2500);
    assert_eq!((ids[0], ids[2499]), (1, 2500));
    let playlist = store.create_playlist("Unlimited").unwrap();
    store.add_playlist(playlist, &[5000]).unwrap();
    store
        .add_playlist_sources(playlist, &directories, &[1, 1])
        .unwrap();
    let first = store.playlist_entries_page(playlist, 0, 2).unwrap();
    assert_eq!(first.total, 5000);
    assert_eq!(
        first
            .rows
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [5000, 1]
    );
    let last = store.playlist_entries_page(playlist, 4999, 1).unwrap();
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
        4999
    );
    store
        .add_playlist_sources(playlist, &directories, &[])
        .unwrap();
    assert_eq!(
        store.playlist_entries_page(playlist, 0, 0).unwrap().total,
        5000
    );
    let empty = store.create_playlist("Atomic").unwrap();
    assert!(
        store
            .add_playlist_sources(empty, &directories, &[99999])
            .is_err()
    );
    assert_eq!(store.playlist_entries_page(empty, 0, 0).unwrap().total, 0);
}

#[test]
fn bulk_missing_cleanup_spans_pages_and_preserves_playlist_reordering() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<1000) INSERT INTO tracks(path,missing) SELECT '/music/'||n||'.wav',n<=500 FROM ids").unwrap();
    let playlist = store.create_playlist("Availability").unwrap();
    store.add_playlist(playlist, &[1000]).unwrap();
    store
        .add_playlist_sources(playlist, &[PathBuf::from("/music")], &[])
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
    let page = store.playlist_entries_page(playlist, 0, 1).unwrap();
    assert_eq!(page.total, 500);
    assert_eq!(page.rows[0].track_id, 1000);
    store.move_playlist_entry(page.rows[0].id, 499).unwrap();
    assert_eq!(
        store.playlist_entries_page(playlist, 499, 1).unwrap().rows[0].track_id,
        1000
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
    for id in 6..=(crate::model::MAX_QUERY_ROWS + 3) {
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
            .track_batch(None, None, None, None, crate::model::MAX_QUERY_ROWS + 1)
            .is_err()
    );
    let first = store
        .track_batch(None, None, None, None, crate::model::MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(first.rows.len(), crate::model::MAX_QUERY_ROWS);
    assert_eq!(first.next, Some(crate::model::MAX_QUERY_ROWS as i64));
    let tail = store
        .track_batch(None, None, None, first.next, crate::model::MAX_QUERY_ROWS)
        .unwrap();
    let expected_tail: Vec<_> = ((crate::model::MAX_QUERY_ROWS + 1)
        ..=(crate::model::MAX_QUERY_ROWS + 3))
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
    let summaries = store.playlist_summary_batch(None, 2).unwrap();
    assert_eq!(
        summaries.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [first, second]
    );
    assert_eq!(summaries.rows[0].entry_count, 4);
    let tail = store.playlist_summary_batch(summaries.next, 2).unwrap();
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
    let entries = store.playlist_entry_batch(first, None, 2).unwrap();
    assert_eq!(
        entries.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        entries.next,
        Some(PlaylistEntryCursor { position: 0, id: 2 })
    );
    let tail_entries = store.playlist_entry_batch(first, entries.next, 2).unwrap();
    assert_eq!(
        tail_entries
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [3, 4]
    );
    assert!(tail_entries.next.is_none());
    assert!(store.playlist_entry_batch(999, None, 1).is_err());
}
