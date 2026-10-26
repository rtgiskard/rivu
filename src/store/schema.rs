use super::Store;
use super::cache::TrackCache;
use anyhow::{Context, Result, bail};
use rivu_protocol::model::DatabaseOptimization;
use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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
        Self::configure_connection(&c)?;
        let empty =
            rivu_format::database::inspect(&c)? == rivu_format::database::DatabaseState::Empty;
        if !empty {
            rivu_format::database::validate_structure(&c)?;
        }
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        c.execute_batch("PRAGMA synchronous=NORMAL;")?;
        if empty {
            rivu_format::database::create_current(&c)?;
        }
        let store = Self {
            conn: c,
            revision_owned_counts: false,
            track_cache: RefCell::new(TrackCache {
                entries: HashMap::new(),
                capacity: (crate::config::DEFAULT_PAGE_SIZE as usize).saturating_mul(2),
                clock: 0,
            }),
            count_cache: RefCell::new(None),
            duplicate_group_count_cache: RefCell::new(None),
            scalar_counts: RefCell::new(super::cache::ScalarCounts::default()),
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
    fn bitrate_precision_constraint_and_data_survive_restart() -> Result<()> {
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
        store
            .conn
            .execute("UPDATE tracks SET bitrate_exact=1 WHERE id=1", [])?;
        drop(store);
        let store = Store::open(&path)?;
        let track = store.track(1)?.unwrap();
        assert!(track.bitrate_exact);
        assert_eq!(track.bitrate_bps, Some(128000));
        Ok(())
    }

    #[test]
    fn foreign_sqlite_named_tables_are_rejected_before_changing_database_mode() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("foreign.db");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE sqlitex_data(value BLOB); INSERT INTO sqlitex_data VALUES(X'01FE');",
        )?;
        let schema: i64 = connection.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
        let journal: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        assert!(Store::open(&path).is_err());
        assert_eq!(
            connection.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?,
            schema
        );
        assert_eq!(
            connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?,
            journal
        );
        assert_eq!(
            connection.query_row("SELECT value FROM sqlitex_data", [], |row| {
                row.get::<_, Vec<u8>>(0)
            })?,
            [1, 254]
        );
        Ok(())
    }

    #[test]
    fn incompatible_databases_are_rejected_without_changing_schema_or_data() -> Result<()> {
        for version in [None, Some(0), Some(1), Some(3)] {
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
            let entries = store.playlist_entries_page(playlist, None, 0, 24)?;
            let connection = &store.conn;
            connection.execute_batch(
                "CREATE TABLE preserved_data(value BLOB NOT NULL);
                 INSERT INTO preserved_data VALUES(X'0001FEFF');",
            )?;
            if let Some(version) = version {
                connection.execute(
                    "UPDATE rivu_meta SET format_version=?1 WHERE singleton=1",
                    [version],
                )?;
            } else {
                connection.execute("DELETE FROM rivu_meta WHERE singleton=1", [])?;
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
            assert_eq!(
                connection.query_row(
                    "SELECT (SELECT format_version FROM rivu_meta WHERE singleton=1)",
                    [],
                    |row| row.get::<_, Option<i64>>(0)
                )?,
                version
            );
        }
        Ok(())
    }
    #[test]
    fn incompatible_same_version_structure_is_rejected_before_journal_changes() -> Result<()> {
        for incompatible in [
            "DELETE FROM playlists WHERE id=0;",
            "CREATE UNIQUE INDEX incompatible_membership ON playlist_entries(playlist_id,track_id);",
            "DROP INDEX playlist_order;",
        ] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("library.db");
            let conn = Connection::open(&path)?;
            rivu_format::database::create_current(&conn)?;
            conn.execute_batch(
                "INSERT INTO tracks(id,path,title_override) VALUES(1,'one','preserved');",
            )?;
            conn.execute_batch(incompatible)?;
            let schema: i64 = conn.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
            let journal: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
            let changes = conn.total_changes();
            assert!(Store::open(&path).is_err(), "{incompatible}");
            assert_eq!(
                conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?,
                schema
            );
            assert_eq!(
                conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?,
                journal
            );
            assert_eq!(conn.total_changes(), changes);
            assert_eq!(
                conn.query_row("SELECT title_override FROM tracks WHERE id=1", [], |row| {
                    row.get::<_, String>(0)
                })?,
                "preserved"
            );
        }
        Ok(())
    }
}
