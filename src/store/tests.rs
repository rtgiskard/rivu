use super::Store;
use crate::model::{
    AlbumSort, CueSegment, DecoderHint, MediaInfo, PlaylistEntryRow, QueryStamp, Track,
};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

struct SqlVmSteps<'connection> {
    connection: &'connection Connection,
    steps: Box<std::cell::Cell<u64>>,
}

impl<'connection> SqlVmSteps<'connection> {
    fn new(connection: &'connection Connection) -> Self {
        let mut counter = Self {
            connection,
            steps: Box::new(std::cell::Cell::new(0)),
        };
        // This fixture owns the connection and has no other progress handler.
        // The boxed counter stays at a stable address until Drop unregisters it.
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                connection.handle(),
                1,
                Some(Self::increment),
                std::ptr::from_mut(counter.steps.as_mut()).cast(),
            );
        }
        counter
    }

    unsafe extern "C" fn increment(context: *mut std::ffi::c_void) -> std::ffi::c_int {
        // SQLite calls synchronously on this connection's thread while the
        // owning guard and its boxed Cell are alive. Never unwind through C.
        let steps = unsafe { &*context.cast::<std::cell::Cell<u64>>() };
        steps.set(steps.get().saturating_add(1));
        0
    }

    fn take(&self) -> u64 {
        self.steps.replace(0)
    }
}

impl Drop for SqlVmSteps<'_> {
    fn drop(&mut self) {
        // Detach before releasing the callback's boxed context, also on panic.
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                self.connection.handle(),
                0,
                None,
                std::ptr::null_mut(),
            );
        }
    }
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
            "INSERT INTO queue_entries(track_id,position)
             SELECT id,2*(id-1) FROM tracks UNION ALL SELECT id,2*(id-1)+1 FROM tracks;
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
            .playlist_entry_batch(
                playlist_id,
                None,
                after,
                crate::model::MAX_QUERY_ROWS,
                QueryStamp::default(),
            )
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
            bitrate_exact: false,
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

fn search_store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("search.db")).unwrap();
    for (id, path, title, artist, album) in [
        (
            1,
            "/music/directory-only/a.wav",
            "Needle Title",
            "Singer",
            "Shared",
        ),
        (
            2,
            "/music/directory-only/b.wav",
            "Other",
            "Needle Artist",
            "Shared",
        ),
        (
            3,
            "/music/directory-only/c.wav",
            "Other",
            "Singer",
            "Needle Album",
        ),
        (
            4,
            "/music/directory-only/needle%_\\file.wav",
            "Other",
            "Singer",
            "Shared",
        ),
        (
            5,
            "/music/directory-only/z.wav",
            "Other",
            "Singer",
            "Shared",
        ),
    ] {
        store
            .conn
            .execute(
                "INSERT INTO tracks(id,path,raw_title,raw_artist,raw_album) VALUES(?1,?2,?3,?4,?5)",
                params![id, path, title, artist, album],
            )
            .unwrap();
    }
    store
        .enqueue_tracks(&[5, 1, 5, 2, 1, 3, 5, 4, 5, 1])
        .unwrap();
    (directory, store)
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
    for offset in [0, 2] {
        let entries = store
            .playlist_entries_page(playlist, Some("needle"), offset, 2)
            .unwrap();
        assert_eq!(entries.total, 4);
        for (index, entry) in entries.rows.iter().enumerate() {
            assert_eq!(
                (entry.position, entry.offset),
                (offset + index + 1, offset + index)
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
    for _ in 0..2 {
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
            (entry_matches[0].id, 1, 0, 4),
            (entry_matches[3].id, 4, 3, 4)
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
        4
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
            .playlist_entries_page(playlist, Some("needle"), 4, 2)
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
        (0, 0, 5)
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
    let key = crate::model::AlbumKey {
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
fn albums_are_directory_scoped_compilations_with_ordered_whole_queue_actions() {
    use crate::model::{AlbumKey, MAX_QUERY_ROWS};
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
    use crate::model::AlbumKey;
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
        .track_id_for_source(Path::new("/music/old/source.flac"), None)
        .unwrap()
        .unwrap();
    let target_id = store
        .track_id_for_source(&target.path, None)
        .unwrap()
        .unwrap();
    let peer_id = store
        .track_id_for_source(&peer.path, None)
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

#[test]
fn playback_precision_upgrade_is_source_conditional_and_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("precision.db");
    let store = Store::open(&database).unwrap();
    let mut record = rec("/music/source.flac", "source", "Source", 10.0);
    record.media.bitrate_bps = Some(128_000);
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let track = all_tracks(&store).remove(0);
    store.enqueue_tracks(&[track.id]).unwrap();
    let source = store.playback_source(&track).unwrap().unwrap();
    assert!(!store.upgrade_bitrate(&source, 192_000, false).unwrap());
    assert!(!store.upgrade_bitrate(&source, 0, true).unwrap());
    let before = store.queue_state().unwrap();
    // Precision itself is content, even when the numerical value is unchanged.
    assert!(store.upgrade_bitrate(&source, 128_000, true).unwrap());
    let exact = store.track(track.id).unwrap().unwrap();
    assert!(exact.bitrate_exact);
    assert!(
        store.queue_page(None, 0, 10).unwrap().rows[0]
            .track
            .bitrate_exact
    );
    assert!(
        store
            .library_view_page(None, None, None, crate::model::LibrarySort::Id, 0, 10)
            .unwrap()
            .rows[0]
            .bitrate_exact
    );
    assert!(
        store
            .known_source(&record.path)
            .unwrap()
            .unwrap()
            .media
            .unwrap()
            .bitrate_exact
    );
    let revision = store.queue_state().unwrap();
    assert!(revision.revision > before.revision);
    assert!(!store.upgrade_bitrate(&source, 256_000, true).unwrap());
    assert!(!store.upgrade_bitrate(&source, 256_000, false).unwrap());
    assert_eq!(store.queue_state().unwrap(), revision);
    assert!(store.playback_source(&exact).unwrap().is_none());

    // Ordinary and forced probes can both arrive after the playback result.
    for bitrate in [Some(96_000), None] {
        record.media.bitrate_bps = bitrate;
        assert!(
            !store
                .apply_scan(&scan(vec![record.clone()]))
                .unwrap()
                .changed
        );
        assert_eq!(store.track(track.id).unwrap().unwrap(), exact);
        assert_eq!(store.queue_state().unwrap(), revision);
    }
    drop(store);
    let store = Store::open(&database).unwrap();
    assert!(store.track(track.id).unwrap().unwrap().bitrate_exact);
    record.fingerprint = Some(blake3::hash(b"replacement").to_hex().to_string());
    record.modified_ns += 1;
    record.media.bitrate_bps = Some(64_000);
    assert!(
        store
            .apply_scan(&scan(vec![record.clone()]))
            .unwrap()
            .changed
    );
    let replacement = store.track(track.id).unwrap().unwrap();
    assert_eq!(replacement.bitrate_bps, Some(64_000));
    assert!(!replacement.bitrate_exact);
    assert!(!store.upgrade_bitrate(&source, 128_000, true).unwrap());
    record.media.bitrate_exact = true;
    assert!(store.apply_scan(&scan(vec![record])).unwrap().changed);
    assert!(store.track(track.id).unwrap().unwrap().bitrate_exact);
}

#[test]
fn playback_precision_fill_and_cue_guard_preserve_source_identity() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = rec("/music/source.flac", "source", "Source", 10.0);
    record.fingerprint = None;
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let track = all_tracks(&store).remove(0);
    let source = store.playback_source(&track).unwrap().unwrap();
    assert!(store.upgrade_bitrate(&source, 128_000, false).unwrap());
    assert!(!store.upgrade_bitrate(&source, 192_000, false).unwrap());
    assert!(store.upgrade_bitrate(&source, 192_000, true).unwrap());
    assert!(
        !store
            .apply_scan(&scan(vec![record.clone()]))
            .unwrap()
            .changed
    );
    record.modified_ns += 1;
    store.apply_scan(&scan(vec![record])).unwrap();
    assert_eq!(store.track(track.id).unwrap().unwrap().bitrate_bps, None);
    assert!(!store.upgrade_bitrate(&source, 192_000, true).unwrap());

    let mut cue = rec("/music/cue.flac", "cue", "Cue", 10.0);
    cue.cue = Some(CueSegment {
        sheet: "/music/album.cue".into(),
        number: 1,
        start_frame: 0,
        end_frame: Some(750),
    });
    store.apply_scan(&scan(vec![cue.clone()])).unwrap();
    let id = store
        .track_id_for_source(Path::new("/music/album.cue"), Some(1))
        .unwrap()
        .unwrap();
    let source = store
        .playback_source(&store.track(id).unwrap().unwrap())
        .unwrap()
        .unwrap();
    assert!(store.upgrade_bitrate(&source, 128_000, true).unwrap());
    cue.cue.as_mut().unwrap().end_frame = Some(700);
    store.apply_scan(&scan(vec![cue])).unwrap();
    assert!(!store.track(id).unwrap().unwrap().bitrate_exact);
    assert!(!store.upgrade_bitrate(&source, 192_000, true).unwrap());
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
    let page_before_hint = store.queue_page(None, 0, 10).unwrap();
    store
        .edit_track(native, None, None, None, Some(DecoderHint::Native))
        .unwrap();
    store
        .edit_track(ffmpeg, None, None, None, Some(DecoderHint::Ffmpeg))
        .unwrap();
    expected[0].decoder_hint = DecoderHint::Native;
    expected[1].decoder_hint = DecoderHint::Ffmpeg;
    assert_eq!(store.queue_state().unwrap(), queue_before_hint);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page_before_hint);
    assert!(
        !store
            .edit_track(native, None, None, None, Some(DecoderHint::Native))
            .unwrap()
            .changed
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
    assert_eq!(s.move_playlist_entry(entries[2].id, 0).unwrap(), Some(pl));
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
    assert_eq!(
        store.move_playlist_entry(before[2].id, 0).unwrap(),
        Some(playlist)
    );
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
    record.fingerprint = Some(blake3::hash(b"replacement").to_hex().to_string());
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
        store.stored_scan_files(&[PathBuf::from("/")]).unwrap()[0]
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
fn playlist_name_conflicts_preserve_catalog_entries_and_queue() {
    let (_directory, store) = search_store();
    let (mix, _) = store.create_playlist_from_queue("Mix").unwrap();
    let other = store.create_playlist("Other").unwrap();
    let catalog = store.playlist_summary_page(None, 0, 20).unwrap();
    let entries = playlist_entries(&store, mix);
    let queue = store.queue_page(None, 0, 20).unwrap();

    assert!(store.create_playlist("Mix").is_err());
    assert!(store.create_playlist_with_tracks("Mix", &[1]).is_err());
    assert!(store.create_playlist_from_queue("Mix").is_err());
    assert!(store.rename_playlist(other, "Mix").is_err());
    assert!(!store.rename_playlist(mix, "Mix").unwrap());
    assert_eq!(store.playlist_summary_page(None, 0, 20).unwrap(), catalog);
    assert_eq!(playlist_entries(&store, mix), entries);
    assert_eq!(store.queue_page(None, 0, 20).unwrap(), queue);

    store.create_playlist("mix").unwrap();
    store.create_playlist(" Mix ").unwrap();
    assert!(store.rename_playlist(mix, "Renamed").unwrap());
    let reused = store.create_playlist("Mix").unwrap();
    store.delete_playlist(reused).unwrap();
    assert!(store.rename_playlist(other, "Mix").unwrap());
}

#[test]
fn legacy_duplicate_playlist_names_remain_readable_and_can_be_resolved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("library.db");
    let store = Store::open(&path).unwrap();
    store
        .conn
        .execute_batch("INSERT INTO playlists(id,name) VALUES(1,'Mix'),(2,'Mix')")
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.playlist_ids().unwrap(), [1, 2]);
    assert!(!store.rename_playlist(1, "Mix").unwrap());
    assert!(store.create_playlist("Mix").is_err());
    assert!(store.rename_playlist(2, "Other").unwrap());
    assert!(store.rename_playlist(2, "Mix").is_err());
    store.delete_playlist(1).unwrap();
    assert!(store.rename_playlist(2, "Mix").unwrap());
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
    let known = store.stored_scan_files(&[PathBuf::from("/")]).unwrap();
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
fn invalid_fingerprints_roll_back_complete_and_incomplete_scans() {
    for incomplete in [false, true] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut original = rec("/music/old.wav", "old", "Original", 10.0);
        store.apply_scan(&scan(vec![original.clone()])).unwrap();
        let id = store
            .track_id_for_source(&original.path, None)
            .unwrap()
            .unwrap();
        store.set_favorite(&[id], true).unwrap();
        let before = store.track(id).unwrap().unwrap();
        original.media.title = "Uncommitted".into();
        let mut invalid = rec("/music/new.wav", "new", "New", 10.0);
        invalid.fingerprint = Some("not-a-fingerprint".into());
        let mut result = scan(vec![original, invalid]);
        result.scan_incomplete = incomplete;
        assert!(store.apply_scan(&result).is_err());
        assert_eq!(store.track_count().unwrap(), 1);
        assert_eq!(store.track(id).unwrap().unwrap(), before);
        store.track_cache.borrow_mut().entries.clear();
        assert_eq!(store.track(id).unwrap().unwrap(), before);
    }
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
fn ambiguous_cue_relocation_preserves_ids_and_user_data_on_both_sides() {
    let directory = tempfile::tempdir().unwrap();
    for (old_count, new_count) in [(2, 1), (1, 2), (2, 2)] {
        for reverse in [false, true] {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let make = |prefix: &str, index| {
                segment(
                    directory
                        .path()
                        .join(format!("{prefix}-{index}.cue"))
                        .to_str()
                        .unwrap(),
                    1,
                    0,
                    Some(4500),
                )
            };
            let mut result = scan((0..old_count).map(|index| make("old", index)).collect());
            result.roots = vec![directory.path().to_path_buf()];
            store.apply_scan(&result).unwrap();
            let old_ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
            store.set_favorite(&old_ids, true).unwrap();
            for id in &old_ids {
                store
                    .edit_track(*id, Some("Owned title"), None, None, None)
                    .unwrap();
            }
            let playlist = store.create_playlist("Owned references").unwrap();
            store.add_playlist(playlist, &old_ids).unwrap();
            result.records = (0..new_count).map(|index| make("new", index)).collect();
            if reverse {
                result.records.reverse();
            }
            store.apply_scan(&result).unwrap();
            let tracks = all_tracks(&store);
            assert_eq!(tracks.len(), old_count + new_count);
            for id in &old_ids {
                let old = store.track(*id).unwrap().unwrap();
                assert!(old.missing && old.favorite);
                assert_eq!(old.title, "Owned title");
            }
            for new in tracks.iter().filter(|track| !old_ids.contains(&track.id)) {
                assert!(!new.missing && !new.favorite);
                assert_eq!(new.title, "Track 1");
            }
            assert_eq!(
                playlist_entries(&store, playlist)
                    .iter()
                    .map(|entry| entry.track_id)
                    .collect::<Vec<_>>(),
                old_ids
            );
        }
    }
}

#[test]
fn exact_cue_identities_do_not_count_as_new_relocation_destinations() {
    let directory = tempfile::tempdir().unwrap();
    for reverse in [false, true] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let make = |name: &str| {
            segment(
                directory.path().join(name).to_str().unwrap(),
                1,
                0,
                Some(4500),
            )
        };
        let old = make("old.cue");
        let exact = make("existing.cue");
        let mut result = scan(vec![old.clone(), exact.clone()]);
        result.roots = vec![directory.path().to_path_buf()];
        store.apply_scan(&result).unwrap();
        let old_id = store
            .track_id_for_source(&old.cue.as_ref().unwrap().sheet, Some(1))
            .unwrap()
            .unwrap();
        let exact_id = store
            .track_id_for_source(&exact.cue.as_ref().unwrap().sheet, Some(1))
            .unwrap()
            .unwrap();
        store
            .edit_track(old_id, Some("Migrated title"), None, None, None)
            .unwrap();
        store
            .edit_track(exact_id, Some("Exact title"), None, None, None)
            .unwrap();
        let destination = make("new.cue");
        result.records = vec![destination.clone(), exact.clone()];
        if reverse {
            result.records.reverse();
        }
        store.apply_scan(&result).unwrap();
        assert_eq!(all_tracks(&store).len(), 2);
        let migrated = store.track(old_id).unwrap().unwrap();
        assert_eq!(migrated.cue, destination.cue);
        assert_eq!(migrated.title, "Migrated title");
        assert!(!migrated.missing);
        let preserved = store.track(exact_id).unwrap().unwrap();
        assert_eq!(preserved.cue, exact.cue);
        assert_eq!(preserved.title, "Exact title");
        assert!(!preserved.missing);
    }
}

#[test]
fn cue_relocation_keeps_unique_segments_when_another_segment_is_ambiguous() {
    let directory = tempfile::tempdir().unwrap();
    let old_sheet = directory.path().join("old.cue");
    let duplicate_sheet = directory.path().join("duplicate.cue");
    let new_sheet = directory.path().join("new.cue");
    let store = Store::open(Path::new(":memory:")).unwrap();
    let make = |sheet: &Path, number| {
        segment(
            sheet.to_str().unwrap(),
            number,
            u64::from(number - 1) * 4500,
            (number < 3).then_some(u64::from(number) * 4500),
        )
    };
    let mut original = scan((1..=3).map(|number| make(&old_sheet, number)).collect());
    original.roots = vec![directory.path().to_path_buf()];
    original.records.push(make(&duplicate_sheet, 2));
    store.apply_scan(&original).unwrap();
    let ids: Vec<_> = (1..=3)
        .map(|number| {
            all_tracks(&store)
                .iter()
                .find(|track| {
                    track
                        .cue
                        .as_ref()
                        .is_some_and(|cue| cue.sheet == old_sheet && cue.number == number)
                })
                .unwrap()
                .id
        })
        .collect();
    store.set_favorite(&ids, true).unwrap();
    let playlist = store.create_playlist("Saved segments").unwrap();
    store.add_playlist(playlist, &ids).unwrap();
    let mut moved = scan(
        (1..=3)
            .rev()
            .map(|number| make(&new_sheet, number))
            .collect(),
    );
    moved.roots = original.roots;
    store.apply_scan(&moved).unwrap();
    let tracks = all_tracks(&store);
    for number in 1..=3 {
        let track = tracks
            .iter()
            .find(|track| {
                track
                    .cue
                    .as_ref()
                    .is_some_and(|cue| cue.sheet == new_sheet && cue.number == number)
            })
            .unwrap();
        let old_id = ids[(number - 1) as usize];
        if number == 2 {
            assert_ne!(track.id, old_id);
            assert!(!track.favorite);
            let old = store.track(old_id).unwrap().unwrap();
            assert!(old.missing && old.favorite);
        } else {
            assert_eq!(track.id, old_id);
            assert!(track.favorite && !track.missing);
        }
    }
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        ids
    );
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
        .directory_page(Path::new("/music"), None, None, None, 0, 10)
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
            .queue_page(None, 0, 2)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.track)
            .collect::<Vec<_>>(),
        [known.clone(), ranking.rows[0].clone()]
    );
}

#[test]
fn playlist_pages_and_batches_preserve_effective_audio_metadata() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut whole = rec("/music/shared.wav", "shared", "Whole", 180.0);
    whole.media.codec = "pcm".into();
    let mut bounded = segment("/music/a.cue", 1, 1500, Some(3750));
    bounded.media.duration = Some(30.0);
    bounded.media.codec = "pcm".into();
    let mut tail = segment("/music/a.cue", 2, 3750, None);
    tail.media.duration = Some(130.0);
    tail.media.codec = "pcm".into();
    let mut unknown = rec("/music/unknown.opus", "unknown", "Unknown", 1.0);
    unknown.media.duration = None;
    unknown.media.codec = "opus".into();
    store
        .apply_scan(&scan(vec![whole, bounded, tail, unknown]))
        .unwrap();
    let tracks = all_tracks(&store);
    let ids: Vec<_> = ["Track 2", "Unknown", "Track 1", "Whole"]
        .iter()
        .map(|title| {
            tracks
                .iter()
                .find(|track| track.title == *title)
                .unwrap()
                .id
        })
        .collect();
    store
        .conn
        .execute(
            "UPDATE tracks SET favorite=1,missing=1 WHERE id=?1",
            [ids[2]],
        )
        .unwrap();
    let playlist = store.create_playlist_with_tracks("metadata", &ids).unwrap();
    let expected = [
        (Some(130.0), "pcm"),
        (None, "opus"),
        (Some(30.0), "pcm"),
        (Some(180.0), "pcm"),
    ];
    for query in [None, Some("Track")] {
        let indices: &[usize] = if query.is_some() {
            &[0, 2]
        } else {
            &[0, 1, 2, 3]
        };
        let mut after = None;
        for (offset, &position) in indices.iter().enumerate() {
            let page = store
                .playlist_entries_page(playlist, query, offset, 1)
                .unwrap();
            assert_eq!(page.total, indices.len());
            assert_eq!(page.rows.len(), 1);
            let row = &page.rows[0];
            assert_eq!(row.track_id, ids[position]);
            assert_eq!(row.position, position);
            assert_eq!(row.offset, offset);
            assert_eq!((row.duration, row.codec.as_str()), expected[position]);
            assert_eq!(row.favorite, position == 2);
            assert_eq!(row.missing, position == 2);
            let batch = store
                .playlist_entry_batch(playlist, query, after, 1, QueryStamp::default())
                .unwrap();
            assert_eq!(batch.rows, page.rows);
            assert_eq!(batch.next.is_some(), offset + 1 < indices.len());
            after = batch.next;
        }
    }
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
fn paged_totals_follow_local_writes_external_commits_and_rollback() {
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
    let external = Connection::open(&path).unwrap();
    external
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
                crate::model::LibrarySort::Id,
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
    assert_eq!(
        store.queue_page(None, 4999, 1).unwrap().rows[0].track_id,
        5000
    );
    let playlist = store.create_playlist("Unlimited").unwrap();
    store.add_playlist(playlist, &[5000]).unwrap();
    store
        .add_playlist_sources(playlist, &directories, &[1, 1])
        .unwrap();
    let first = store.playlist_entries_page(playlist, None, 0, 2).unwrap();
    assert_eq!(first.total, 5000);
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
        4999
    );
    store
        .add_playlist_sources(playlist, &directories, &[])
        .unwrap();
    assert_eq!(
        store
            .playlist_entries_page(playlist, None, 0, 0)
            .unwrap()
            .total,
        5000
    );
    let empty = store.create_playlist("Atomic").unwrap();
    assert!(
        store
            .add_playlist_sources(empty, &directories, &[99999])
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
    let page = store.playlist_entries_page(playlist, None, 0, 1).unwrap();
    assert_eq!(page.total, 500);
    assert_eq!(page.rows[0].track_id, 1000);
    assert_eq!(
        store.move_playlist_entry(page.rows[0].id, 499).unwrap(),
        Some(playlist)
    );
    assert_eq!(
        store
            .playlist_entries_page(playlist, None, 499, 1)
            .unwrap()
            .rows[0]
            .track_id,
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
    assert_eq!(store.queue_state().unwrap().revision, state.revision);
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

#[test]
fn unsupported_database_versions_are_rejected_without_migration() {
    fs::create_dir_all(".cache").unwrap();
    let directory = tempfile::tempdir_in(".cache").unwrap();
    for version in [6, 15] {
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

#[test]
fn library_locate_matches_tree_and_flat_projection_without_identity_fallback() {
    use crate::model::{DirectoryRow, LibraryLocateResult, LibrarySort};
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
