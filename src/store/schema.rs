use super::Store;
use super::cache::TrackCache;
use crate::model::DatabaseOptimization;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub(super) const TRACK_TABLE: &str = "CREATE TABLE tracks(
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
    bits_per_sample INTEGER,release_date TEXT,
    favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
    decoder_hint INTEGER NOT NULL DEFAULT 0 CHECK(decoder_hint IN (0,1,2))
);";

pub(super) const TRACK_INDEXES_AND_TRIGGER: &str = "
    CREATE INDEX tracks_fingerprint ON tracks(fingerprint);
    CREATE UNIQUE INDEX tracks_path ON tracks(path) WHERE cue_sheet IS NULL;
    CREATE INDEX tracks_directory ON tracks(path);
    CREATE UNIQUE INDEX tracks_cue ON tracks(cue_sheet,cue_number) WHERE cue_sheet IS NOT NULL;
    CREATE INDEX tracks_recent ON tracks(last_played DESC,id DESC) WHERE last_played IS NOT NULL;
    CREATE INDEX tracks_album ON tracks(COALESCE(album_override,raw_album), CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(disc_number,4294967295) END, CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(track_number,4294967295) END, COALESCE(title_override,raw_title));
    CREATE INDEX tracks_most_played ON tracks(play_count DESC);
    CREATE TRIGGER queue_metadata AFTER UPDATE OF raw_title,raw_artist,raw_album,
        title_override,artist_override,album_override,duration,favorite,missing,
        play_count,codec,bitrate_bps ON tracks
    WHEN (COALESCE(old.title_override,old.raw_title) IS NOT COALESCE(new.title_override,new.raw_title)
        OR COALESCE(old.artist_override,old.raw_artist) IS NOT COALESCE(new.artist_override,new.raw_artist)
        OR COALESCE(old.album_override,old.raw_album) IS NOT COALESCE(new.album_override,new.raw_album)
        OR old.duration IS NOT new.duration
        OR old.favorite IS NOT new.favorite OR old.missing IS NOT new.missing
        OR old.play_count IS NOT new.play_count OR old.codec IS NOT new.codec
        OR old.bitrate_bps IS NOT new.bitrate_bps)
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
        INSERT INTO queue_meta(singleton) VALUES(1);
        INSERT INTO schema_meta(key,value) VALUES('version',10);",
    )?;
    tx.execute_batch(TRACK_INDEXES_AND_TRIGGER)?;
    tx.commit()?;
    Ok(())
}

const MIGRATE_QUEUE_TO_V10: &str = "DROP TRIGGER IF EXISTS queue_insert;
    DROP TRIGGER IF EXISTS queue_delete;
    DROP TRIGGER IF EXISTS queue_move;
    DELETE FROM schema_meta WHERE key='compact_pending';
    UPDATE schema_meta SET value=10 WHERE key='version';";

fn migrate_legacy_to_v10(conn: &Connection, version: i64) -> Result<()> {
    conn.create_scalar_function(
        "fingerprint_blob",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let value = ctx.get_raw(0);
            if value == rusqlite::types::ValueRef::Null {
                return Ok(None);
            }
            blake3::Hash::from_hex(value.as_str()?)
                .map(|hash| Some(*hash.as_bytes()))
                .map_err(|error| {
                    rusqlite::Error::UserFunctionError(
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
                            .into(),
                    )
                })
        },
    )?;
    // Rebuild only tracks, with FK actions disabled outside the atomic transaction.
    // Queue entries/current selection and every other table retain their identities.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        let sequence: Option<i64> = tx
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='tracks'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute_batch(&TRACK_TABLE.replacen(
            "CREATE TABLE tracks(",
            "CREATE TABLE tracks_v9(",
            1,
        ))?;
        let columns = "id,path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,title_override,artist_override,album_override,duration,codec,channels,sample_rate,missing,play_count,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,bits_per_sample,release_date,favorite";
        let select = columns.replace("fingerprint", "fingerprint_blob(fingerprint)");
        let hint = if version == 7 {
            "0"
        } else {
            "CASE decoder_hint WHEN 'auto' THEN 0 WHEN 'native' THEN 1 WHEN 'ffmpeg' THEN 2 END"
        };
        tx.execute_batch(&format!(
            "INSERT INTO tracks_v9({columns},decoder_hint) SELECT {select},{hint} FROM tracks;
             DROP TRIGGER queue_metadata;
             DROP TABLE tracks;
             ALTER TABLE tracks_v9 RENAME TO tracks;"
        ))?;
        if let Some(sequence) = sequence {
            tx.execute(
                "UPDATE sqlite_sequence SET seq=?1 WHERE name='tracks'",
                [sequence],
            )?;
        }
        tx.execute_batch(TRACK_INDEXES_AND_TRIGGER)?;
        tx.execute_batch(
            "DROP INDEX queue_order;
             CREATE INDEX queue_order ON queue_entries(position);
             DROP INDEX playlist_order;
             CREATE INDEX playlist_order ON playlist_entries(playlist_id,position);
             CREATE INDEX IF NOT EXISTS playlist_track_lookup ON playlist_entries(track_id,playlist_id);"
        )?;
        tx.execute_batch(MIGRATE_QUEUE_TO_V10)?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.remove_function("fingerprint_blob", 1)?;
    result
}

fn migrate_to_v10(conn: &Connection) -> Result<()> {
    // Keep all occurrence IDs, current selection and revision. A version boundary
    // prevents older Store implementations from accepting a trigger-free schema.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(MIGRATE_QUEUE_TO_V10)?;
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
                .context("Unsupported library database; remove the old database and rescan")?;
            match version {
                7 | 8 => migrate_legacy_to_v10(&c, version).with_context(|| {
                    format!("Migrate library database from version {version} to 10")
                })?,
                9 => migrate_to_v10(&c).context("Migrate library database from version 9 to 10")?,
                10 => {}
                _ => bail!(
                    "Unsupported database version {version}; remove the old database and rescan"
                ),
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
pub(super) const LEGACY_QUEUE_TRIGGERS: &str = "
    CREATE TRIGGER queue_insert AFTER INSERT ON queue_entries BEGIN
        UPDATE queue_meta SET revision=revision+1;
    END;
    CREATE TRIGGER queue_delete AFTER DELETE ON queue_entries BEGIN
        UPDATE queue_meta SET revision=revision+1;
    END;
    CREATE TRIGGER queue_move AFTER UPDATE OF position ON queue_entries
    WHEN old.position<>new.position BEGIN
        UPDATE queue_meta SET revision=revision+1;
    END;";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v9_migration_preserves_occurrences_current_and_revision() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("queue.db");
        let store = Store::open(&path)?;
        store
            .conn
            .execute_batch("INSERT INTO tracks(path) VALUES('/a'),('/b');")?;
        store.enqueue_tracks(&[1, 2, 1])?;
        let ids = store.queue_ids()?;
        store.set_queue_current(Some(ids[2]))?;
        store.conn.execute_batch(LEGACY_QUEUE_TRIGGERS)?;
        store
            .conn
            .execute("UPDATE schema_meta SET value=9 WHERE key='version'", [])?;
        let state = store.queue_state()?;
        let page = store.queue_page(0, 10)?;
        drop(store);

        let store = Store::open(&path)?;
        assert_eq!(store.queue_state()?, state);
        assert_eq!(store.queue_page(0, 10)?, page);
        store.reorder_queue(&ids)?;
        store.move_queue_entry(ids[0], 0)?;
        assert_eq!(store.queue_state()?, state);
        store.move_queue_entry(ids[0], 2)?;
        let changed = store.queue_state()?;
        assert!(changed.revision > state.revision);
        assert_eq!(changed.current_id, state.current_id);
        store.set_favorite(&[1], true)?;
        assert!(store.queue_state()?.revision > changed.revision);
        let state = store.queue_state()?;
        let page = store.queue_page(0, 10)?;
        drop(store);
        let reopened = Store::open(&path)?;
        assert_eq!(reopened.queue_state()?, state);
        assert_eq!(reopened.queue_page(0, 10)?, page);
        Ok(())
    }

    #[test]
    fn failed_v10_migration_rolls_back_trigger_removal_and_version() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(LEGACY_QUEUE_TRIGGERS)?;
        store.conn.execute_batch(
            "UPDATE schema_meta SET value=9 WHERE key='version';
             CREATE TEMP TRIGGER reject_schema_version AFTER UPDATE ON schema_meta
             BEGIN SELECT RAISE(ABORT,'injected migration failure'); END;",
        )?;
        assert!(migrate_to_v10(&store.conn).is_err());
        store
            .conn
            .execute_batch("DROP TRIGGER reject_schema_version;")?;
        store
            .conn
            .execute("INSERT INTO tracks(path) VALUES('/a')", [])?;
        let before = store.queue_state()?;
        store.conn.execute(
            "INSERT INTO queue_entries(track_id,position) VALUES(1,0)",
            [],
        )?;
        assert!(store.queue_state()?.revision > before.revision);
        let version: i64 = store.conn.query_row(
            "SELECT value FROM schema_meta WHERE key='version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(version, 9);
        Ok(())
    }
}
