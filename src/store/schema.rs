use super::Store;
use super::cache::TrackCache;
use crate::model::DatabaseOptimization;
use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

fn create_schema(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE schema_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
        CREATE TABLE tracks(
            id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL,fingerprint TEXT,
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
            favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1))
        );
        CREATE INDEX tracks_fingerprint ON tracks(fingerprint);
        CREATE UNIQUE INDEX tracks_path ON tracks(path) WHERE cue_sheet IS NULL;
        CREATE UNIQUE INDEX tracks_cue ON tracks(cue_sheet,cue_number) WHERE cue_sheet IS NOT NULL;
        CREATE INDEX tracks_recent ON tracks(last_played DESC,id DESC) WHERE last_played IS NOT NULL;
        CREATE TABLE playlists(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL);
        CREATE TABLE playlist_entries(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
            track_id INTEGER NOT NULL,position INTEGER NOT NULL
        );
        CREATE INDEX playlist_order ON playlist_entries(playlist_id,position,id);
        CREATE UNIQUE INDEX playlist_track ON playlist_entries(playlist_id,track_id);
        CREATE INDEX playlist_track_lookup ON playlist_entries(track_id,playlist_id,position);
        CREATE TABLE queue_entries(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            position INTEGER NOT NULL
        );
        CREATE INDEX queue_order ON queue_entries(position,id);
        CREATE INDEX queue_track ON queue_entries(track_id);
        CREATE TABLE queue_meta(
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            current_id INTEGER REFERENCES queue_entries(id) ON DELETE SET NULL,
            revision INTEGER NOT NULL DEFAULT 0
        );
        INSERT INTO queue_meta(singleton) VALUES(1);
        CREATE TRIGGER queue_insert AFTER INSERT ON queue_entries BEGIN
            UPDATE queue_meta SET revision=revision+1;
        END;
        CREATE TRIGGER queue_delete AFTER DELETE ON queue_entries BEGIN
            UPDATE queue_meta SET revision=revision+1;
        END;
        CREATE TRIGGER queue_move AFTER UPDATE OF position ON queue_entries
        WHEN old.position<>new.position BEGIN
            UPDATE queue_meta SET revision=revision+1;
        END;
        CREATE TRIGGER queue_metadata AFTER UPDATE OF raw_title,raw_artist,raw_album,
            title_override,artist_override,album_override,duration,favorite,missing,
            play_count,codec,bitrate_bps ON tracks
        WHEN (old.raw_title IS NOT new.raw_title OR old.raw_artist IS NOT new.raw_artist
            OR old.raw_album IS NOT new.raw_album OR old.title_override IS NOT new.title_override
            OR old.artist_override IS NOT new.artist_override
            OR old.album_override IS NOT new.album_override OR old.duration IS NOT new.duration
            OR old.favorite IS NOT new.favorite OR old.missing IS NOT new.missing
            OR old.play_count IS NOT new.play_count OR old.codec IS NOT new.codec
            OR old.bitrate_bps IS NOT new.bitrate_bps)
            AND EXISTS(SELECT 1 FROM queue_entries WHERE track_id=new.id) BEGIN
            UPDATE queue_meta SET revision=revision+1;
        END;
        INSERT INTO schema_meta(key,value) VALUES('version',7);",
    )?;
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
            if version != 7 {
                bail!("Unsupported database version {version}; remove the old database and rescan");
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
        let pending: bool = store.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_meta WHERE key='compact_pending' AND value=1)",
            [],
            |row| row.get(0),
        )?;
        if pending && let Err(error) = store.optimize() {
            tracing::warn!(error = %error, "database_maintenance_pending");
        }
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
        self.conn.execute(
            "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('compact_pending',1)",
            [],
        )?;
        self.conn.execute_batch("VACUUM; PRAGMA optimize;")?;
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            bail!(
                "Database checkpoint blocked by another reader; retry optimization after closing it"
            );
        }
        self.conn
            .execute("DELETE FROM schema_meta WHERE key='compact_pending'", [])?;
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            self.conn.execute(
                "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('compact_pending',1)",
                [],
            )?;
            bail!("Database checkpoint blocked after maintenance; retry optimization");
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
