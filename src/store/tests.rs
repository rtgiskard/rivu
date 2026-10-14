use super::Store;
use crate::model::{
    CueSegment, DecoderHint, MediaInfo, PlaylistEntryCursor, PlaylistEntryRow, Track,
};
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
use crate::library::{ScanRecord, ScanResult, ScanUncertainty};
fn rec(path: &str, hash: &str, title: &str, dur: f64) -> ScanRecord {
    ScanRecord {
        path: path.into(),
        cue: None,
        size: 1,
        modified_ns: 1,
        fingerprint: Some(blake3::hash(hash.as_bytes()).to_hex().to_string()),
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
        uncertainties: Vec::new(),
    }
}
fn db(name: &str) -> (PathBuf, Store) {
    let p = std::env::temp_dir().join(format!("rivu-{name}-{}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    (p.clone(), Store::open(&p).unwrap())
}

#[test]
fn decoder_hint_persists_without_overwriting_track_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("decoder-hint.db");
    let store = Store::open(&path).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/native.wav", "native", "Native", 10.),
            rec("/music/ffmpeg.wav", "ffmpeg", "FFmpeg", 20.),
        ]))
        .unwrap();
    let mut expected = all_tracks(&store);
    assert!(
        expected
            .iter()
            .all(|track| track.decoder_hint == DecoderHint::Auto)
    );
    let native = expected[0].id;
    let ffmpeg = expected[1].id;
    store.set_favorite(&[native], true).unwrap();
    store
        .edit_track(native, Some("Custom"), None, None, None)
        .unwrap();
    store.mark_played(native, 42).unwrap();
    store.increment_play_count(native).unwrap();
    expected[0] = store.track(native).unwrap().unwrap(); // populate the cache
    store.enqueue_tracks(&[native, native, ffmpeg]).unwrap();
    let queue_before_hint = store.queue_state().unwrap();
    let page_before_hint = store.queue_page(0, 10).unwrap();
    store
        .edit_track(native, None, None, None, Some(DecoderHint::Native))
        .unwrap();
    store
        .edit_track(ffmpeg, None, None, None, Some(DecoderHint::Ffmpeg))
        .unwrap();
    expected[0].decoder_hint = DecoderHint::Native;
    expected[1].decoder_hint = DecoderHint::Ffmpeg;
    assert_eq!(store.queue_state().unwrap(), queue_before_hint);
    assert_eq!(store.queue_page(0, 10).unwrap(), page_before_hint);
    assert!(
        !store
            .edit_track(native, None, None, None, Some(DecoderHint::Native))
            .unwrap()
    );
    assert_eq!(store.track(native).unwrap().unwrap(), expected[0]);
    assert!(
        store
            .edit_track(999, Some("Missing"), None, None, Some(DecoderHint::Native))
            .is_err()
    );
    assert_eq!(all_tracks(&store), expected);
    assert!(
        store
            .conn
            .execute("UPDATE tracks SET decoder_hint=3 WHERE id=?1", [native])
            .is_err()
    );
    assert!(
        store
            .conn
            .execute("UPDATE tracks SET decoder_hint=NULL WHERE id=?1", [native])
            .is_err()
    );
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(all_tracks(&reopened), expected);
    reopened
        .edit_track(native, None, None, None, Some(DecoderHint::Auto))
        .unwrap();
    assert_eq!(
        reopened.track(native).unwrap().unwrap().decoder_hint,
        DecoderHint::Auto
    );
}

#[test]
fn metadata_patch_omitting_hint_preserves_concurrent_preference() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata-patch.db");
    let store = Store::open(&path).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.wav", "a", "Raw", 10.)]))
        .unwrap();
    let baseline = all_tracks(&store)[0].clone();
    let concurrent = Store::open(&path).unwrap();
    concurrent
        .edit_track(baseline.id, None, None, None, Some(DecoderHint::Ffmpeg))
        .unwrap();
    store
        .edit_track(baseline.id, Some("Edited"), None, None, None)
        .unwrap();
    let saved = store.track(baseline.id).unwrap().unwrap();
    assert_eq!(saved.title, "Edited");
    assert_eq!(saved.decoder_hint, DecoderHint::Ffmpeg);
    assert_eq!(saved.artist, baseline.artist);
    assert_eq!(saved.album, baseline.album);
}

#[test]
fn combined_metadata_patch_commits_or_fails_as_one_update() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.wav", "a", "Raw", 10.)]))
        .unwrap();
    let id = all_tracks(&store)[0].id;
    store.set_favorite(&[id], true).unwrap();
    store.mark_played(id, 42).unwrap();
    store.increment_play_count(id).unwrap();
    let before = store.track(id).unwrap().unwrap();
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER reject_decoder_hint BEFORE UPDATE OF decoder_hint ON tracks
         WHEN NEW.decoder_hint=2 BEGIN SELECT RAISE(ABORT,'rejected hint'); END;",
        )
        .unwrap();
    assert!(
        store
            .edit_track(
                id,
                Some("Edited"),
                Some("Artist"),
                Some("Album"),
                Some(DecoderHint::Ffmpeg)
            )
            .is_err()
    );
    assert_eq!(store.track(id).unwrap().unwrap(), before);
    store
        .conn
        .execute_batch("DROP TRIGGER reject_decoder_hint")
        .unwrap();
    store
        .edit_track(
            id,
            Some("Edited"),
            Some("Artist"),
            Some("Album"),
            Some(DecoderHint::Ffmpeg),
        )
        .unwrap();
    let mut expected = before;
    expected.title = "Edited".into();
    expected.artist = "Artist".into();
    expected.album = "Album".into();
    expected.decoder_hint = DecoderHint::Ffmpeg;
    assert_eq!(store.track(id).unwrap().unwrap(), expected);
    assert!(
        store
            .edit_track(
                id + 1,
                Some("Missing"),
                Some("Artist"),
                Some("Album"),
                Some(DecoderHint::Native)
            )
            .is_err()
    );
    assert_eq!(all_tracks(&store), vec![expected]);
}

// Rebuild the supported legacy tracks definition, not a v9 table with an old label.
fn legacy_schema(store: &Store, version: i64, patched_indexes: bool) {
    let hint = if version == 7 {
        ""
    } else {
        ",decoder_hint TEXT NOT NULL DEFAULT 'auto' CHECK(decoder_hint IN ('auto','native','ffmpeg'))"
    };
    let table = super::schema::TRACK_TABLE
        .replacen("CREATE TABLE tracks(", "CREATE TABLE tracks_legacy(", 1)
        .replace("fingerprint BLOB CHECK(fingerprint IS NULL OR (typeof(fingerprint)='blob' AND length(fingerprint)=32))", "fingerprint TEXT")
        .replace("    relocation_independent INTEGER NOT NULL DEFAULT 0 CHECK(relocation_independent IN (0,1)),\n", "")
        .replace(",\n    decoder_hint INTEGER NOT NULL DEFAULT 0 CHECK(decoder_hint IN (0,1,2))", hint);
    let columns = "id,path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,title_override,artist_override,album_override,duration,codec,channels,sample_rate,missing,play_count,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,bits_per_sample,release_date,favorite";
    let select = columns.replace(
        "fingerprint",
        "CASE WHEN fingerprint IS NULL THEN NULL ELSE lower(hex(fingerprint)) END",
    );
    let hint_columns = if version == 7 { "" } else { ",decoder_hint" };
    let hint_select = if version == 7 {
        ""
    } else {
        ",CASE decoder_hint WHEN 0 THEN 'auto' WHEN 1 THEN 'native' WHEN 2 THEN 'ffmpeg' END"
    };
    store
        .conn
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    let tx = store.conn.unchecked_transaction().unwrap();
    let sequence: i64 = tx
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='tracks'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    tx.execute_batch(&table).unwrap();
    tx.execute_batch(&format!(
        "INSERT INTO tracks_legacy({columns}{hint_columns}) SELECT {select}{hint_select} FROM tracks;
         DROP TRIGGER queue_metadata;
         DROP TABLE tracks;
         ALTER TABLE tracks_legacy RENAME TO tracks;
         DROP INDEX queue_order;
         CREATE INDEX queue_order ON queue_entries(position,id);
         DROP INDEX playlist_order;
         CREATE INDEX playlist_order ON playlist_entries(playlist_id,position,id);
         UPDATE schema_meta SET value={version} WHERE key='version';"
    )).unwrap();
    tx.execute(
        "UPDATE sqlite_sequence SET seq=?1 WHERE name='tracks'",
        [sequence],
    )
    .unwrap();
    tx.execute_batch(super::schema::TRACK_INDEXES_AND_TRIGGER)
        .unwrap();
    tx.execute_batch("DROP INDEX tracks_album; DROP INDEX tracks_most_played;")
        .unwrap();
    if !patched_indexes {
        tx.execute_batch("DROP INDEX tracks_directory; DROP INDEX playlist_track_lookup;")
            .unwrap();
    }
    tx.execute_batch(super::schema::LEGACY_QUEUE_TRIGGERS)
        .unwrap();
    tx.commit().unwrap();
    store
        .conn
        .pragma_update(None, "foreign_keys", "ON")
        .unwrap();
}

#[test]
fn supported_migrations_preserve_library_playlist_queue_and_current_identity() {
    for (version, patched) in [(7, false), (7, true), (8, true)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.db");
        let store = Store::open(&path).unwrap();
        let records = vec![
            rec("/music/a.wav", "a", "Raw", 10.),
            segment("/music/a.cue", 1, 0, Some(4500)),
        ];
        store.apply_scan(&scan(records)).unwrap();
        let id = all_tracks(&store)[0].id;
        store
            .edit_track(id, Some("Custom"), Some(""), None, None)
            .unwrap();
        if version == 8 {
            store
                .edit_track(id, None, None, None, Some(DecoderHint::Ffmpeg))
                .unwrap();
        }
        store.set_favorite(&[id], true).unwrap();
        store.mark_played(id, 42).unwrap();
        store.increment_play_count(id).unwrap();
        let playlist = store.create_playlist("Saved").unwrap();
        store.add_playlist(playlist, &[2, id]).unwrap();
        store.enqueue_tracks(&[id, 2, id]).unwrap();
        let queue_ids = store.queue_ids().unwrap();
        store.set_queue_current(Some(queue_ids[2])).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO tracks(id,path) VALUES(3,'/music/unhashed.wav')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute("INSERT INTO tracks(id,path) VALUES(100,'deleted')", [])
            .unwrap();
        store.remove_tracks(&[100]).unwrap();
        let expected_tracks = all_tracks(&store);
        let expected_known = store.known_files(&[PathBuf::from("/")]).unwrap();
        let expected_playlist = playlist_entries(&store, playlist);
        let expected_queue = store.queue_state().unwrap();
        let expected_page = store.queue_page(0, 10).unwrap();
        legacy_schema(&store, version, patched);
        drop(store);

        let migrated = Store::open(&path).unwrap();
        assert_eq!(
            migrated
                .conn
                .query_row(
                    "SELECT value FROM schema_meta WHERE key='version'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            11
        );
        assert_eq!(all_tracks(&migrated), expected_tracks);
        assert_eq!(
            migrated
                .known_files(&[PathBuf::from("/")])
                .unwrap()
                .iter()
                .map(|file| &file.fingerprint)
                .collect::<Vec<_>>(),
            expected_known
                .iter()
                .map(|file| &file.fingerprint)
                .collect::<Vec<_>>()
        );
        assert_eq!(playlist_entries(&migrated, playlist), expected_playlist);
        assert_eq!(migrated.queue_ids().unwrap(), queue_ids);
        assert_eq!(migrated.queue_state().unwrap(), expected_queue);
        assert_eq!(migrated.queue_page(0, 10).unwrap(), expected_page);
        assert_eq!(migrated.conn.query_row("SELECT COUNT(*) FROM tracks WHERE typeof(fingerprint)='blob' AND length(fingerprint)=32 AND typeof(decoder_hint)='integer'", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
        assert!(
            !migrated
                .edit_track(id, Some("Custom"), Some(""), None, None)
                .unwrap()
        );
        migrated.enqueue_sources(&["/music".into()], &[]).unwrap();
        migrated
            .conn
            .execute("INSERT INTO tracks(path) VALUES('/music/new.wav')", [])
            .unwrap();
        assert!(migrated.conn.last_insert_rowid() > 100);
        assert_eq!(
            migrated
                .conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        drop(migrated);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.track(id).unwrap().unwrap(), expected_tracks[0]);
    }
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
fn title_patch_keeps_unedited_metadata_live_across_rescans() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = rec("/music/a.mp3", "same", "Raw title", 100.);
    record.media.artist = "Raw artist".into();
    record.media.album = "Raw album".into();
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let id = all_tracks(&store)[0].id;
    // Prime the cache before editing and scanning.
    assert_eq!(store.track(id).unwrap().unwrap().artist, "Raw artist");
    store
        .edit_track(id, Some("Edited title"), None, None, None)
        .unwrap();
    record.media.title = "Rescanned title".into();
    record.media.artist = "Rescanned artist".into();
    record.media.album = "Rescanned album".into();
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.title, "Edited title");
    assert_eq!(track.artist, "Rescanned artist");
    assert_eq!(track.album, "Rescanned album");

    // Empty text is an actual override, unlike an omitted field.
    store.edit_track(id, None, Some(""), None, None).unwrap();
    record.media.artist = "Another artist".into();
    record.media.album = "Another album".into();
    store.apply_scan(&scan(vec![record])).unwrap();
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.title, "Edited title");
    assert_eq!(track.artist, "");
    assert_eq!(track.album, "Another album");
}

#[test]
fn moved_id_preserves_override_and_delete_does_not_reuse() {
    let (p, s) = db("identity");
    s.apply_scan(&scan(vec![rec("/music/a.mp3", "same", "Raw", 100.)]))
        .unwrap();
    let id = all_tracks(&s)[0].id;
    s.edit_track(
        id,
        Some("Override"),
        Some("A"),
        Some("B"),
        Some(DecoderHint::Ffmpeg),
    )
    .unwrap();
    s.apply_scan(&scan(vec![rec("/music/b.mp3", "same", "Changed", 100.)]))
        .unwrap();
    assert_eq!(all_tracks(&s)[0].id, id);
    assert_eq!(all_tracks(&s)[0].title, "Override");
    assert_eq!(all_tracks(&s)[0].decoder_hint, DecoderHint::Ffmpeg);
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
        store.known_files(&[PathBuf::from("/")]).unwrap()[0]
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
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(all_tracks(&reopened)[0].last_played, Some(42));
    reopened.optimize().unwrap();
    assert_eq!(all_tracks(&reopened)[0].last_played, Some(42));
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
        .edit_track(ids[1], Some("Custom"), Some("Artist"), Some("Album"), None)
        .unwrap();
    store.mark_played(ids[1], 10).unwrap();
    store.increment_play_count(ids[1]).unwrap();
    let playlist = store.create_playlist("Segments").unwrap();
    store
        .add_playlist(playlist, &[ids[1], ids[2], ids[1], ids[3]])
        .unwrap();
    store
        .edit_track(ids[1], None, None, None, Some(DecoderHint::Native))
        .unwrap();
    store
        .edit_track(ids[2], None, None, None, Some(DecoderHint::Ffmpeg))
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
    assert_eq!(after[0].decoder_hint, DecoderHint::Auto);
    assert_eq!(after[1].decoder_hint, DecoderHint::Native);
    assert_eq!(after[2].decoder_hint, DecoderHint::Ffmpeg);
    assert_eq!(after[3].decoder_hint, DecoderHint::Auto);
    for index in [0, 2, 3] {
        assert_eq!(
            (after[index].play_count, after[index].last_played),
            (0, None)
        );
        assert!(!after[index].missing);
    }
    let known = store.known_files(&[PathBuf::from("/")]).unwrap();
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
fn uncertain_scopes_preserve_owned_rows_and_reject_relocation_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let old_path = root.join("failed/whole.wav");
    let old_sheet = root.join("failed/old.cue");
    let copy_path = root.join("copy.wav");
    let new_sheet = root.join("new.cue");
    for (uncertainty, protect_file, protect_cue) in [
        (ScanUncertainty::Root { root_index: 0 }, true, true),
        (
            ScanUncertainty::Subtree {
                root_index: 0,
                path: root.join("failed"),
            },
            true,
            true,
        ),
        (ScanUncertainty::File(old_path.clone()), true, false),
        (ScanUncertainty::CueSheet(old_sheet.clone()), false, true),
    ] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let old_file = rec(old_path.to_str().unwrap(), "whole", "Original", 10.0);
        let old_cue = segment(old_sheet.to_str().unwrap(), 1, 0, Some(4500));
        let mut baseline = scan(vec![old_file.clone(), old_cue.clone()]);
        baseline.roots = vec![root.clone()];
        store.apply_scan(&baseline).unwrap();
        let file_id = store.track_id_for_source(&old_path, None).unwrap().unwrap();
        let cue_id = store
            .track_id_for_source(&old_sheet, Some(1))
            .unwrap()
            .unwrap();
        store.set_favorite(&[file_id, cue_id], true).unwrap();
        let copy = rec(copy_path.to_str().unwrap(), "whole", "Copy", 10.0);
        let new_cue = segment(new_sheet.to_str().unwrap(), 1, 0, Some(4500));
        let mut partial = scan(vec![copy, new_cue]);
        partial.roots = vec![root.clone()];
        partial.uncertainties.push(uncertainty);
        store.apply_scan(&partial).unwrap();
        if protect_file {
            let preserved = store.track(file_id).unwrap().unwrap();
            assert_eq!(preserved.path, old_file.path);
            assert!(!preserved.missing);
            assert!(preserved.favorite);
            assert_ne!(
                store
                    .track_id_for_source(&copy_path, None)
                    .unwrap()
                    .unwrap(),
                file_id
            );
        }
        if protect_cue {
            let preserved = store.track(cue_id).unwrap().unwrap();
            assert_eq!(preserved.cue, old_cue.cue);
            assert!(!preserved.missing);
            assert!(preserved.favorite);
            assert_ne!(
                store
                    .track_id_for_source(&new_sheet, Some(1))
                    .unwrap()
                    .unwrap(),
                cue_id
            );
        }
        store.remove_missing_tracks().unwrap();
        assert!(store.track(file_id).unwrap().is_some());
        assert!(store.track(cue_id).unwrap().is_some());
    }
}

#[test]
fn nearest_explicit_root_owns_uncertain_directory_coverage() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let sibling = rec("/music/deep/sibling.wav", "sibling", "Sibling", 10.0);
    let nested = rec("/music/deep/nested/absent.wav", "nested", "Nested", 10.0);
    let beyond = segment("/music/deep/nested/beyond/hidden.cue", 1, 0, Some(4500));
    store
        .apply_scan(&scan(vec![sibling.clone(), nested.clone(), beyond.clone()]))
        .unwrap();
    let sibling_id = store
        .track_id_for_source(&sibling.path, None)
        .unwrap()
        .unwrap();
    let nested_id = store
        .track_id_for_source(&nested.path, None)
        .unwrap()
        .unwrap();
    let beyond_id = store
        .track_id_for_source(&beyond.cue.as_ref().unwrap().sheet, Some(1))
        .unwrap()
        .unwrap();
    let mut partial = scan(Vec::new());
    partial.roots.push("/music/deep/nested".into());
    partial.uncertainties = vec![
        ScanUncertainty::Root { root_index: 0 },
        ScanUncertainty::Subtree {
            root_index: 0,
            path: "/music/deep/nested".into(),
        },
        ScanUncertainty::Subtree {
            root_index: 1,
            path: "/music/deep/nested/beyond".into(),
        },
    ];
    store.apply_scan(&partial).unwrap();
    assert!(!store.track(sibling_id).unwrap().unwrap().missing);
    assert!(store.track(nested_id).unwrap().unwrap().missing);
    assert!(!store.track(beyond_id).unwrap().unwrap().missing);
}

#[test]
fn incomplete_scan_does_not_relocate_unknown_sources_outside_requested_roots() {
    let directory = tempfile::tempdir().unwrap();
    let old_path = directory.path().join("outside/old.wav");
    let requested = directory.path().join("requested");
    let copy_path = requested.join("copy.wav");
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec(
            old_path.to_str().unwrap(),
            "same",
            "Original",
            10.0,
        )]))
        .unwrap();
    let id = store.track_id_for_source(&old_path, None).unwrap().unwrap();
    let mut partial = scan(vec![rec(copy_path.to_str().unwrap(), "same", "Copy", 10.0)]);
    partial.roots = vec![requested];
    partial.scan_incomplete = true;
    store.apply_scan(&partial).unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().path, old_path);
    assert_ne!(
        store
            .track_id_for_source(&copy_path, None)
            .unwrap()
            .unwrap(),
        id
    );
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
        store
            .edit_track(id, Some("Override"), Some("Artist"), Some("Album"), None)
            .unwrap();
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
fn library_audio_metadata_survives_all_display_projections() {
    use crate::model::{DirectoryRow, LibrarySort};
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut known = rec("/music/known.flac", "known", "Known", 123.5);
    known.media.codec = "flac".into();
    known.media.bitrate_bps = Some(921_600);
    known.media.artist = "Artist".into();
    known.media.album = "Album".into();
    let mut unknown = rec("/music/unknown.opus", "unknown", "Unknown", 1.0);
    unknown.media.codec = "opus".into();
    unknown.media.duration = None;
    store.apply_scan(&scan(vec![known, unknown])).unwrap();
    let library = store
        .library_view_page(None, None, None, LibrarySort::Id, 0, 10)
        .unwrap();
    assert_eq!(library.total, 2);
    let known = &library.rows[0];
    assert_eq!(known.codec, "flac");
    assert_eq!(known.bitrate_bps, Some(921_600));
    assert_eq!(known.duration, Some(123.5));
    assert_eq!((&*known.artist, &*known.album), ("Artist", "Album"));
    let unknown = &library.rows[1];
    assert_eq!(unknown.codec, "opus");
    assert_eq!(unknown.bitrate_bps, None);
    assert_eq!(unknown.duration, None);
    store.increment_play_count(unknown.id).unwrap();
    let ranking = store
        .library_view_page(None, None, None, LibrarySort::MostPlayed, 0, 10)
        .unwrap();
    assert_eq!(ranking.rows[0].id, unknown.id);
    assert_eq!(ranking.rows[0].codec, unknown.codec);
    assert_eq!(ranking.rows[0].bitrate_bps, None);
    assert_eq!(ranking.rows[1], *known);
    let directory = store
        .directory_page(Path::new("/music"), None, None, 0, 10)
        .unwrap();
    assert_eq!(directory.rows[0], DirectoryRow::Track(known.clone()));
    assert_eq!(
        directory.rows[1],
        DirectoryRow::Track(ranking.rows[0].clone())
    );
    let ids = [known.id, unknown.id];
    store.enqueue_tracks(&ids).unwrap();
    assert_eq!(
        store
            .queue_page(0, 2)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.track)
            .collect::<Vec<_>>(),
        [known.clone(), ranking.rows[0].clone()]
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
        .directory_page(Path::new("/music"), None, Some(true), 0, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(
        page.rows,
        [DirectoryRow::Directory {
            path: "/music/mixed".into()
        }]
    );
    let page = store
        .directory_page(Path::new("/music"), None, Some(true), 1, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(matches!(&page.rows[..], [DirectoryRow::Track(row)] if row.id == ids[1]));
    let page = store
        .directory_page(Path::new("/music"), None, Some(true), 3, 1)
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(page.rows.is_empty());
    let page = store
        .directory_page(Path::new("/music/mixed"), None, Some(true), 0, 10)
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(
        page.rows,
        [DirectoryRow::Directory {
            path: "/music/mixed/deep".into()
        }]
    );
    let page = store
        .directory_page(Path::new("/music/mixed/deep"), None, Some(true), 0, 10)
        .unwrap();
    assert!(matches!(&page.rows[..], [DirectoryRow::Track(row)] if row.id == ids[0]));
    let page = store
        .directory_page(Path::new("/music/empty"), None, Some(true), 0, 10)
        .unwrap();
    assert_eq!(page.total, 0);
    assert!(page.rows.is_empty());
    let page = store
        .directory_page(Path::new("/music"), None, Some(false), 0, 10)
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
        .directory_page(Path::new("/music"), None, None, 0, 10)
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
    use crate::model::DirectoryRow;
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
        .track_id_for_source(Path::new("/music/a/deep/one.wav"), None)
        .unwrap()
        .unwrap();
    let other = store
        .track_id_for_source(Path::new("/music/b/other.wav"), None)
        .unwrap()
        .unwrap();
    store.set_favorite(&[favorite, other], true).unwrap();
    let first = store
        .directory_page(Path::new("/music"), Some("NEEDLE"), None, 0, 2)
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
        .directory_page(Path::new("/music"), Some("needle"), None, 2, 2)
        .unwrap();
    assert_eq!(tail.total, 4);
    assert!(
        matches!(&tail.rows[0], DirectoryRow::Directory { path } if path == Path::new("/music/b"))
    );
    assert!(matches!(&tail.rows[1], DirectoryRow::Track(row) if row.title == "Needle Direct"));
    let selected = store
        .directory_page(Path::new("/music"), Some("needle"), Some(true), 0, 10)
        .unwrap();
    assert_eq!(selected.total, 1);
    assert_eq!(
        selected.rows,
        [DirectoryRow::Directory {
            path: "/music/a".into()
        }]
    );
    let ancestor = store
        .directory_page(Path::new("/music/a/"), Some("needle"), Some(true), 0, 10)
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
            0,
            10,
        )
        .unwrap();
    assert_eq!(non_favorites.total, 1);
    assert!(
        matches!(&non_favorites.rows[..], [DirectoryRow::Track(row)] if row.title == "Needle Two")
    );
    let root = store
        .directory_page(Path::new(""), Some("needle"), Some(true), 0, 10)
        .unwrap();
    assert_eq!(root.total, 1);
    assert_eq!(root.rows, [DirectoryRow::Directory { path: "/".into() }]);
    for favorite in [None, Some(true), Some(false)] {
        let unfiltered = store
            .directory_page(Path::new("/music"), None, favorite, 1, 10)
            .unwrap();
        let empty = store
            .directory_page(Path::new("/music"), Some(""), favorite, 1, 10)
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
            .directory_page(Path::new("/music"), Some(query), None, offset, limit)
            .unwrap();
        assert_eq!(page.total, if query == "absent" { 0 } else { 4 });
        assert!(page.rows.is_empty());
    }
}

#[test]
fn directory_search_reuses_literal_unicode_metadata_and_path_matching() {
    use crate::model::{DirectoryRow, LibrarySort};
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
            .directory_page(Path::new("/music/search"), Some(query), None, 0, 10)
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
                DirectoryRow::Directory { .. } => panic!("expected only tracks"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            library.rows.iter().map(|row| row.id).collect::<Vec<_>>()
        );
    }
    assert_eq!(
        store
            .directory_page(Path::new("/music/search"), Some("été_100%"), None, 0, 10)
            .unwrap()
            .total,
        2
    );
    assert_eq!(
        store
            .directory_page(Path::new("/music/search"), Some("raw title"), None, 0, 10)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        store
            .directory_page(Path::new("/music/search"), Some("étéX100Z"), None, 0, 10)
            .unwrap()
            .total,
        0
    );
}

#[test]
fn library_root_queries_preserve_path_boundaries_overlap_and_unicode() {
    use crate::model::{LibraryRootCount, LibrarySort};
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
    use crate::model::LibrarySort;
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
    use crate::model::{LibrarySort, MAX_QUERY_ROWS};
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
        .directory_page(Path::new(""), None, None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(root.rows, [DirectoryRow::Directory { path: "/".into() }]);
    let music = store
        .directory_page(Path::new("/music"), None, None, 0, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(music.total, 4);
    assert!(
        matches!(&music.rows[0], DirectoryRow::Directory { path } if path == Path::new("/music/Album"))
    );
    let first = store
        .directory_page(Path::new("/music/Album/"), None, None, 0, usize::MAX)
        .unwrap();
    assert_eq!(first.total, 300);
    assert_eq!(first.rows.len(), MAX_QUERY_ROWS);
    let searched = store
        .directory_page(
            Path::new("/music/Album/"),
            Some("Track "),
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
            MAX_QUERY_ROWS,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(last.rows.len(), 44);
    assert!(matches!(&last.rows[43], DirectoryRow::Track(row) if row.title == "Track 299"));
    let cue_sources = store
        .directory_page(Path::new("/music"), None, None, 2, MAX_QUERY_ROWS)
        .unwrap();
    assert_eq!(cue_sources.rows.len(), 2);
    assert!(
        cue_sources
            .rows
            .iter()
            .all(|row| matches!(row, DirectoryRow::Track(_)))
    );
    let cue_matches = store
        .directory_page(Path::new("/music"), Some("Track "), None, 1, MAX_QUERY_ROWS)
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
            1,
            MAX_QUERY_ROWS,
        )
        .unwrap();
    assert_eq!(cue_match.total, 2);
    assert!(matches!(&cue_match.rows[..], [DirectoryRow::Track(row)] if row.title == "Track 2"));
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
    store.enqueue_sources(&directories, &[1, 1]).unwrap();
    assert_eq!(store.queue_state().unwrap().total, 5000);
    assert_eq!(store.queue_page(4999, 1).unwrap().rows[0].track_id, 5000);
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

#[test]
fn persistent_queue_accepts_entire_large_library_pages_restarts_and_saves_playlist_in_sql() {
    fs::create_dir_all(".cache").unwrap();
    let directory = tempfile::tempdir_in(".cache").unwrap();
    let path = directory.path().join("library.db");
    let store = Store::open(&path).unwrap();
    store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<200000) INSERT INTO tracks(path,raw_title) SELECT '/music/'||n||'.wav','Track '||n FROM ids;").unwrap();
    store
        .enqueue_sources(&[PathBuf::from("/music")], &[])
        .unwrap();
    let state = store.queue_state().unwrap();
    assert_eq!(state.total, 200_000);
    assert!(serde_json::to_vec(&state).unwrap().len() < 160);
    let last = store.queue_page(199_936, 256).unwrap();
    assert_eq!(last.rows.len(), 64);
    assert_eq!(last.rows[63].position, 199_999);
    assert_eq!(last.rows[63].track_id, 200_000);
    assert!(store.queue_page(200_000, 256).unwrap().rows.is_empty());
    let first = store.queue_batch(None, 256).unwrap();
    let second = store.queue_batch(first.next, 256).unwrap();
    assert_eq!(second.rows[0].position, 256);
    let current = last.rows[63].id;
    store.set_queue_current(Some(current)).unwrap();
    assert_eq!(store.queue_state().unwrap().revision, state.revision);
    let playlist = store.create_playlist_from_queue("All").unwrap();
    assert_eq!(
        store
            .playlist_entries_page(playlist, 199_999, 1)
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
    assert!(store.queue_entries(&vec![current; 257]).is_err());
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
        let page = store.queue_page(0, 9).unwrap();
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
        let rows = store.queue_page(0, 8).unwrap().rows;
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
    let playlist = store.create_playlist_from_queue("First order").unwrap();
    assert_eq!(store.playlist_track_ids(playlist, 2).unwrap(), [1, 2]);
    store.set_queue_current(Some(ids[2])).unwrap();
    let before = store.queue_state().unwrap();
    assert!(store.remove_tracks(&[1, 999]).is_err());
    assert_eq!(store.queue_state().unwrap(), before);
    store.remove_tracks(&[1, 1]).unwrap();
    assert_eq!(store.queue_ids().unwrap(), [ids[1]]);
    assert_eq!(store.queue_state().unwrap().current_id, None);
    assert_eq!(store.queue_page(0, 1).unwrap().rows[0].position, 0);
    assert_eq!(store.playlist_track_ids(playlist, 2).unwrap(), [2]);
    store
        .conn
        .execute("UPDATE tracks SET missing=1 WHERE id=2", [])
        .unwrap();
    store.remove_missing_tracks().unwrap();
    assert_eq!(store.queue_state().unwrap().total, 0);
    assert_eq!(
        store.playlist_entries_page(playlist, 0, 1).unwrap().total,
        0
    );
}

#[test]
fn unsupported_database_versions_are_rejected_without_migration() {
    fs::create_dir_all(".cache").unwrap();
    let directory = tempfile::tempdir_in(".cache").unwrap();
    for version in [6, 12] {
        let path = directory.path().join(format!("version-{version}.db"));
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE schema_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);")
            .unwrap();
        connection
            .execute("INSERT INTO schema_meta VALUES('version',?1)", [version])
            .unwrap();
        assert!(Store::open(&path).is_err());
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM schema_meta WHERE key='version'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            version
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='queue_entries'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}
