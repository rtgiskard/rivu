use super::Store;
use rivu_audio::MediaInfo;
use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::model::{AlbumSort, CueSegment, PlaylistEntryRow, QueryStamp, Track};
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

fn all_tracks(store: &Store) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut after = None;
    loop {
        let batch = store
            .track_batch(
                None,
                None,
                None,
                after,
                rivu_protocol::model::MAX_QUERY_ROWS,
            )
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
                rivu_protocol::model::MAX_QUERY_ROWS,
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
use crate::library::{CueRecord, RegisteredCueMetadata, ScanRecord, ScanResult, ScanUncertainty};
fn rec(path: &str, hash: &str, title: &str, dur: f64) -> ScanRecord {
    ScanRecord {
        path: path.into(),
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
        records: records.into_iter().map(std::sync::Arc::new).collect(),
        min_duration_secs: 0,
        errors: vec![],
        skipped_directories: Vec::new(),
        skipped_directories_truncated: false,
        scan_incomplete: false,
        uncertainties: Vec::new(),
        ..Default::default()
    }
}
fn db() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("library.db")).unwrap();
    (directory, store)
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

fn segment(sheet: &str, number: u32, start: u64, end: Option<u64>) -> CueRecord {
    CueRecord::registered(
        std::sync::Arc::new(rec("/music/shared.wav", "shared", "Source", 180.0)),
        CueSegment {
            sheet: sheet.into(),
            number,
            start_frame: start,
            end_frame: end,
        },
        RegisteredCueMetadata {
            title: format!("Track {number}"),
            artist: String::new(),
            album: String::new(),
            track_number: None,
            disc_number: None,
            release_date: None,
        },
        Some(60.0),
    )
}

mod albums;
mod cache;
mod collections;
mod maintenance;
mod other_tracks;
mod playlists;
mod query;
mod queue;
mod scan;
mod scan_reconciliation;
mod tracks;
