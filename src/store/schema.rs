use super::Store;
use super::cache::TrackCache;
use crate::model::DatabaseOptimization;
use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;

const TRACK_TABLE: &str = "CREATE TABLE tracks(
    id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL,
    fingerprint BLOB CHECK(fingerprint IS NULL OR (typeof(fingerprint)='blob' AND length(fingerprint)=32)),
    file_size INTEGER NOT NULL DEFAULT 0,modified_ns INTEGER NOT NULL DEFAULT 0,
    raw_title TEXT NOT NULL DEFAULT '',raw_artist TEXT NOT NULL DEFAULT '',
    raw_album TEXT NOT NULL DEFAULT '',title_override TEXT,artist_override TEXT,
    album_override TEXT,duration REAL,codec TEXT NOT NULL DEFAULT '',
    channels INTEGER NOT NULL DEFAULT 0,sample_rate INTEGER NOT NULL DEFAULT 0,
    missing INTEGER NOT NULL DEFAULT 0,play_count INTEGER NOT NULL DEFAULT 0,
    last_played INTEGER,cue_sheet TEXT,cue_number INTEGER,
    cue_start_frame INTEGER,cue_end_frame INTEGER,
    probe_version INTEGER NOT NULL DEFAULT 0,
    bitrate_bps INTEGER,track_number INTEGER,disc_number INTEGER,
    bitrate_exact INTEGER NOT NULL DEFAULT 0 CHECK(bitrate_exact IN (0,1)),
    bits_per_sample INTEGER,release_date TEXT,
    favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
    relocation_independent INTEGER NOT NULL DEFAULT 0 CHECK(relocation_independent IN (0,1)),
    decoder_hint INTEGER NOT NULL DEFAULT 0 CHECK(decoder_hint IN (0,1,2))
);";

const TRACK_INDEXES: &str = "
    CREATE INDEX tracks_fingerprint ON tracks(fingerprint);
    CREATE UNIQUE INDEX tracks_path ON tracks(path) WHERE cue_sheet IS NULL;
    CREATE INDEX tracks_directory ON tracks(path);
    CREATE UNIQUE INDEX tracks_cue ON tracks(cue_sheet,cue_number) WHERE cue_sheet IS NOT NULL;
    CREATE INDEX tracks_recent ON tracks(last_played DESC,id DESC) WHERE last_played IS NOT NULL;
    CREATE INDEX tracks_album ON tracks(COALESCE(album_override,raw_album), CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(disc_number,4294967295) END, CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(track_number,4294967295) END, COALESCE(title_override,raw_title));
    CREATE INDEX tracks_most_played ON tracks(play_count DESC);";

const QUEUE_METADATA_TRIGGER: &str = "
    CREATE TRIGGER queue_metadata AFTER UPDATE OF raw_title,raw_artist,raw_album,
        title_override,artist_override,album_override,duration,favorite,missing,
        play_count,codec,bitrate_bps,bitrate_exact ON tracks
    WHEN (COALESCE(old.title_override,old.raw_title) IS NOT COALESCE(new.title_override,new.raw_title)
        OR COALESCE(old.artist_override,old.raw_artist) IS NOT COALESCE(new.artist_override,new.raw_artist)
        OR COALESCE(old.album_override,old.raw_album) IS NOT COALESCE(new.album_override,new.raw_album)
        OR old.duration IS NOT new.duration
        OR old.favorite IS NOT new.favorite OR old.missing IS NOT new.missing
        OR old.play_count IS NOT new.play_count OR old.codec IS NOT new.codec
        OR old.bitrate_bps IS NOT new.bitrate_bps OR old.bitrate_exact IS NOT new.bitrate_exact)
        AND EXISTS(SELECT 1 FROM queue_entries WHERE track_id=new.id) BEGIN
        UPDATE queue_meta SET revision=revision+1;
    END;";

fn create_schema(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(TRACK_TABLE)?;
    tx.execute_batch(
        "CREATE TABLE schema_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
        CREATE TABLE playlists(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL);
        CREATE TABLE playlist_entries(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
            track_id INTEGER NOT NULL,position INTEGER NOT NULL
        );
        CREATE INDEX playlist_order ON playlist_entries(playlist_id,position);
        CREATE UNIQUE INDEX playlist_track ON playlist_entries(playlist_id,track_id);
        CREATE INDEX playlist_track_lookup ON playlist_entries(track_id,playlist_id);
        CREATE TABLE queue_entries(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            position INTEGER NOT NULL
        );
        CREATE INDEX queue_order ON queue_entries(position);
        CREATE INDEX queue_track ON queue_entries(track_id);
        CREATE TABLE queue_meta(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            current_id INTEGER REFERENCES queue_entries(id) ON DELETE SET NULL,
            revision INTEGER NOT NULL DEFAULT 0
        );
        INSERT INTO queue_meta(singleton) VALUES(1);",
    )?;
    tx.execute(
        "INSERT INTO schema_meta(key,value) VALUES('version',?1)",
        [SCHEMA_VERSION],
    )?;
    tx.execute_batch(TRACK_INDEXES)?;
    tx.execute_batch(QUEUE_METADATA_TRIGGER)?;
    super::albums::create_summary_schema(&tx)?;
    tx.commit()?;
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent()
            && !p.as_os_str().is_empty()
            && path != Path::new(":memory:")
        {
            fs::create_dir_all(p)?;
        }
        let c =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        c.execute_batch("PRAGMA synchronous=NORMAL;")?;
        let populated: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
            [], |row| row.get(0),
        )?;
        if !populated {
            create_schema(&c)?;
        } else {
            let version: i64 = c
                .query_row(
                    "SELECT value FROM schema_meta WHERE key='version'",
                    [],
                    |row| row.get(0),
                )
                .context("Unsupported library database; back up the database and WAL before replacing it; rescanning cannot restore user state")?;
            if version != SCHEMA_VERSION {
                bail!(
                    "Unsupported database version {version}; back up the database and WAL before replacing it; rescanning cannot restore user state"
                );
            }
        }
        // SQLite's built-in lower() is ASCII-only; preserve frontend Unicode search.
        c.create_scalar_function(
            "lower",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let value = ctx
                    .get_raw(0)
                    .as_str()
                    .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
                Ok(value.to_lowercase())
            },
        )?;
        let store = Self {
            conn: c,
            track_cache: RefCell::new(TrackCache {
                entries: HashMap::new(),
                capacity: (crate::config::DEFAULT_PAGE_SIZE as usize).saturating_mul(2),
                clock: 0,
            }),
            count_cache: RefCell::new(None),
        };
        Ok(store)
    }
    pub fn optimize(&self) -> Result<DatabaseOptimization> {
        let sizes = || -> Result<(u64, u64)> {
            let path = self.conn.path().filter(|path| !path.is_empty());
            let database_size = if let Some(path) = path {
                fs::metadata(path)?.len()
            } else {
                let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
                let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
                (pages * page_size) as u64
            };
            let wal = path.map(|path| format!("{path}-wal"));
            let wal_size = match wal.map(fs::metadata) {
                Some(Ok(metadata)) => metadata.len(),
                Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => 0,
            };
            Ok((database_size, wal_size))
        };
        let (database_bytes_before, wal_bytes_before) = sizes()?;
        self.conn.execute_batch("VACUUM; PRAGMA optimize;")?;
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            bail!(
                "Database checkpoint blocked by another reader; retry optimization after closing it"
            );
        }
        let (database_bytes_after, wal_bytes_after) = sizes()?;
        Ok(DatabaseOptimization {
            database_bytes_before,
            database_bytes_after,
            wal_bytes_before,
            wal_bytes_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_precision_updates_queue_metadata_and_survives_restart() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("library.db");
        let store = Store::open(&path)?;
        store.conn.execute(
            "INSERT INTO tracks(path,bitrate_bps) VALUES('/music/current.flac',128000)",
            [],
        )?;
        assert!(!store.track(1)?.unwrap().bitrate_exact);
        assert!(
            store
                .conn
                .execute("UPDATE tracks SET bitrate_exact=2", [])
                .is_err()
        );
        store.enqueue_tracks(&[1])?;
        let revision = store.queue_state()?.revision;
        store
            .conn
            .execute("UPDATE tracks SET bitrate_exact=1 WHERE id=1", [])?;
        assert!(store.queue_state()?.revision > revision);
        drop(store);
        let store = Store::open(&path)?;
        let track = store.track(1)?.unwrap();
        assert!(track.bitrate_exact);
        assert_eq!(track.bitrate_bps, Some(128000));
        Ok(())
    }

    #[test]
    fn incompatible_databases_are_rejected_without_changing_schema_or_data() -> Result<()> {
        for version in [None, Some(0), Some(2)] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("library.db");
            let store = Store::open(&path)?;
            store.conn.execute_batch(
                "INSERT INTO tracks(id,path,raw_title,title_override,raw_album,album_override,
                    raw_artist,artist_override,favorite,play_count,last_played) VALUES
                    (1,'/music/one.flac','Raw','User title','Raw album','','Raw artist','User artist',1,7,42),
                    (2,'/music/two.flac','Two',NULL,'Real album',NULL,'',NULL,0,0,NULL);",
            )?;
            let playlist = store.create_playlist("Preserved playlist")?;
            store.add_playlist(playlist, &[2, 1])?;
            store.enqueue_tracks(&[1, 2, 1])?;
            store.set_queue_current(Some(store.queue_ids()?[1]))?;
            let tracks = [store.track(1)?, store.track(2)?];
            let queue = store.queue_page(None, 0, 24)?;
            let queue_state = store.queue_state()?;
            let entries = store.playlist_entries_page(playlist, None, 0, 24)?;
            let connection = &store.conn;
            connection.execute_batch(
                "CREATE TABLE preserved_data(value BLOB NOT NULL);
                 INSERT INTO preserved_data VALUES(X'0001FEFF');",
            )?;
            if let Some(version) = version {
                connection.execute(
                    "UPDATE schema_meta SET value=?1 WHERE key='version'",
                    [version],
                )?;
            } else {
                connection.execute_batch("DROP TABLE schema_meta;")?;
            }
            let schema_version: i64 =
                connection.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
            let data_version: i64 =
                connection.query_row("PRAGMA data_version", [], |row| row.get(0))?;

            assert!(Store::open(&path).is_err());
            assert_eq!(
                connection.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?,
                data_version
            );
            assert_eq!([store.track(1)?, store.track(2)?], tracks);
            assert_eq!(store.queue_page(None, 0, 24)?, queue);
            assert_eq!(store.queue_state()?, queue_state);
            assert_eq!(store.playlist_entries_page(playlist, None, 0, 24)?, entries);

            let connection = Connection::open(&path)?;
            assert_eq!(
                connection.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?,
                schema_version
            );
            assert_eq!(
                connection.query_row("SELECT value FROM preserved_data", [], |row| {
                    row.get::<_, Vec<u8>>(0)
                })?,
                [0, 1, 254, 255]
            );
            if let Some(version) = version {
                assert_eq!(
                    connection.query_row(
                        "SELECT value FROM schema_meta WHERE key='version'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )?,
                    version
                );
            }
        }
        Ok(())
    }
}
