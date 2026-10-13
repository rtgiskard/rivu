use super::positions::{PositionScope, close_gaps, move_entry};
use super::tracks::{SOURCE_FILTER, validate_encoded_track_ids};
use super::{Store, batch_limit, changed};
use crate::model::{
    Batch, Page, PlaylistEntryCursor, PlaylistEntryLocation, PlaylistEntryRow, PlaylistSummary,
};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, Transaction, params};
use std::path::PathBuf;

impl Store {
    pub fn playlist_entry(&self, entry_id: i64) -> Result<Option<PlaylistEntryLocation>> {
        // Positions are contiguous. The covering playlist_order index supplies the last
        // position without counting or projecting the entire playlist.
        Ok(self.conn.query_row(
            "SELECT e.id,e.playlist_id,e.track_id,e.position,
                    (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=e.playlist_id)
             FROM playlist_entries e WHERE e.id=?1",
            [entry_id],
            |row| Ok(PlaylistEntryLocation {
                id: row.get(0)?,
                playlist_id: row.get(1)?,
                track_id: row.get(2)?,
                position: row.get::<_, i64>(3)? as usize,
                total: row.get::<_, i64>(4)? as usize,
            }),
        ).optional()?)
    }

    pub fn playlist_summary_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Page<PlaylistSummary>> {
        let total = self
            .conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |row| {
                row.get::<_, i64>(0)
            })? as usize;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        let rows = self.playlist_summary_rows(offset, limit.min(crate::model::MAX_QUERY_ROWS))?;
        Ok(Page { total, rows })
    }

    fn playlist_summary_rows(&self, offset: usize, limit: usize) -> Result<Vec<PlaylistSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id,p.name,(SELECT COUNT(*) FROM playlist_entries e WHERE e.playlist_id=p.id)
             FROM playlists p ORDER BY p.id LIMIT ?1 OFFSET ?2",
        )?;
        Ok(stmt
            .query_map(
                params![
                    limit.min(crate::model::MAX_QUERY_ROWS) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                |r| {
                    Ok(PlaylistSummary {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        entry_count: r.get::<_, i64>(2)? as usize,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_playlist_sources(
        &self,
        playlist_id: i64,
        directories: &[PathBuf],
        track_ids: &[i64],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        if !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
            [playlist_id],
            |row| row.get::<_, bool>(0),
        )? {
            bail!("Playlist not found");
        }
        let (directories, ids) = self.source_parameters(directories, track_ids)?;
        // Assign positions in SQL; playlist size never determines Rust allocation.
        tx.execute(&format!(
            "WITH selected AS (SELECT id FROM tracks WHERE ({SOURCE_FILTER}) AND NOT EXISTS (SELECT 1 FROM playlist_entries e WHERE e.playlist_id=?3 AND e.track_id=tracks.id)), ordered AS (SELECT id,row_number() OVER (ORDER BY id)-1 AS ordinal FROM selected) INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?3,id,(SELECT COALESCE(MAX(position),-1)+1 FROM playlist_entries WHERE playlist_id=?3)+ordinal FROM ordered"
        ), params![directories, ids, playlist_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn playlist_summary_batch(
        &self,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Batch<PlaylistSummary, i64>> {
        let limit = batch_limit(limit)?;
        let mut stmt = if after.is_some() {
            self.conn.prepare(
                "SELECT p.id,p.name,
                        (SELECT COUNT(*) FROM playlist_entries e WHERE e.playlist_id=p.id)
                 FROM playlists p WHERE p.id>?1 ORDER BY p.id LIMIT ?2",
            )?
        } else {
            self.conn.prepare(
                "SELECT p.id,p.name,
                        (SELECT COUNT(*) FROM playlist_entries e WHERE e.playlist_id=p.id)
                 FROM playlists p ORDER BY p.id LIMIT ?1",
            )?
        };
        let mut db_rows = if let Some(after) = after {
            stmt.query(params![after, limit + 1])?
        } else {
            stmt.query(params![limit + 1])?
        };
        let mut rows = Vec::with_capacity(limit as usize);
        let mut has_more = false;
        while let Some(row) = db_rows.next()? {
            if rows.len() == limit as usize {
                has_more = true;
                break;
            }
            rows.push(PlaylistSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                entry_count: row.get::<_, i64>(2)? as usize,
            });
        }
        let next = has_more
            .then(|| rows.last().map(|summary| summary.id))
            .flatten();
        Ok(Batch { rows, next })
    }

    pub fn playlist_entry_batch(
        &self,
        playlist_id: i64,
        after: Option<PlaylistEntryCursor>,
        limit: usize,
    ) -> Result<Batch<PlaylistEntryRow, PlaylistEntryCursor>> {
        let limit = batch_limit(limit)?;
        let exists = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
            [playlist_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            bail!("Playlist not found");
        }
        let mut stmt = if after.is_some() {
            self.conn.prepare(
                "SELECT e.position,e.id,e.track_id,
                        COALESCE(t.title_override,t.raw_title),
                        COALESCE(t.artist_override,t.raw_artist),
                        COALESCE(t.album_override,t.raw_album),
                        COALESCE(t.missing,1)
                 FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id
                 WHERE e.playlist_id=?1 AND (e.position,e.id)>(?2,?3)
                 ORDER BY e.position,e.id LIMIT ?4",
            )?
        } else {
            self.conn.prepare(
                "SELECT e.position,e.id,e.track_id,
                        COALESCE(t.title_override,t.raw_title),
                        COALESCE(t.artist_override,t.raw_artist),
                        COALESCE(t.album_override,t.raw_album),
                        COALESCE(t.missing,1)
                 FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id
                 WHERE e.playlist_id=?1
                 ORDER BY e.position,e.id LIMIT ?2",
            )?
        };
        let mut db_rows = if let Some(after) = after {
            stmt.query(params![playlist_id, after.position, after.id, limit + 1])?
        } else {
            stmt.query(params![playlist_id, limit + 1])?
        };
        let mut rows = Vec::with_capacity(limit as usize);
        let mut last_cursor = None;
        let mut has_more = false;
        while let Some(row) = db_rows.next()? {
            if rows.len() == limit as usize {
                has_more = true;
                break;
            }
            let cursor = PlaylistEntryCursor {
                position: row.get(0)?,
                id: row.get(1)?,
            };
            rows.push(PlaylistEntryRow {
                id: cursor.id,
                track_id: row.get(2)?,
                title: row.get(3)?,
                artist: row.get(4)?,
                album: row.get(5)?,
                missing: row.get::<_, i64>(6)? != 0,
            });
            last_cursor = Some(cursor);
        }
        let next = has_more.then_some(last_cursor).flatten();
        Ok(Batch { rows, next })
    }

    pub fn playlist_entries_page(
        &self,
        playlist_id: i64,
        offset: usize,
        limit: usize,
    ) -> Result<Page<PlaylistEntryRow>> {
        let total: i64 = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1",
            [playlist_id],
            |r| r.get(0),
        ).optional()?.context("Playlist not found")?;
        if limit == 0 {
            return Ok(Page {
                total: total as usize,
                rows: Vec::new(),
            });
        }
        let mut stmt = self.conn.prepare("SELECT e.id,e.track_id,COALESCE(t.title_override,t.raw_title),COALESCE(t.artist_override,t.raw_artist),COALESCE(t.album_override,t.raw_album),COALESCE(t.missing,1) FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id WHERE e.playlist_id=?1 ORDER BY e.position,e.id LIMIT ?2 OFFSET ?3")?;
        let limit = limit.min(crate::model::MAX_QUERY_ROWS) as i64;
        let offset = offset.min(i64::MAX as usize) as i64;
        let rows = stmt
            .query_map(params![playlist_id, limit, offset], |r| {
                Ok(PlaylistEntryRow {
                    id: r.get(0)?,
                    track_id: r.get(1)?,
                    title: r.get(2)?,
                    artist: r.get(3)?,
                    album: r.get(4)?,
                    missing: r.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page {
            total: total as usize,
            rows,
        })
    }

    pub fn playlist_track_ids(&self, playlist_id: i64, limit: usize) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT track_id FROM playlist_entries WHERE playlist_id=?1 ORDER BY position,id LIMIT ?2")?;
        let limit = limit.min(i64::MAX as usize) as i64;
        Ok(stmt
            .query_map(params![playlist_id, limit], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        self.create_playlist_with_tracks(name, &[])
    }

    pub fn create_playlist_with_tracks(&self, name: &str, tracks: &[i64]) -> Result<i64> {
        valid_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO playlists(name)VALUES(?)", [name])?;
        let id = tx.last_insert_rowid();
        append_tracks(&tx, id, tracks)?;
        tx.commit()?;
        Ok(id)
    }
    pub fn create_playlist_from_queue(&self, name: &str) -> Result<i64> {
        valid_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO playlists(name) VALUES(?1)", [name])?;
        let id = tx.last_insert_rowid();
        tx.execute("WITH first AS (SELECT track_id,MIN(position) AS position FROM queue_entries GROUP BY track_id) INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?1,track_id,row_number() OVER (ORDER BY position,track_id)-1 FROM first ORDER BY position,track_id", [id])?;
        tx.commit()?;
        Ok(id)
    }
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<bool> {
        valid_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        exists_playlist(&tx, id)?;
        let updated = tx.execute(
            "UPDATE playlists SET name=?1 WHERE id=?2 AND name IS NOT ?1",
            params![name, id],
        )?;
        tx.commit()?;
        Ok(updated != 0)
    }
    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        changed(
            self.conn
                .execute("DELETE FROM playlists WHERE id=?", [id])?,
            "playlist",
            id,
        )
    }
    pub fn add_playlist(&self, id: i64, tracks: &[i64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        exists_playlist(&tx, id)?;
        append_tracks(&tx, id, tracks)?;
        tx.commit()?;
        Ok(())
    }
    pub fn remove_playlist_entry(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let (pid, p): (i64, i64) = tx
            .query_row(
                "SELECT playlist_id,position FROM playlist_entries WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("playlist entry {id} does not exist"))?;
        tx.execute("DELETE FROM playlist_entries WHERE id=?", [id])?;
        close_gaps(&tx, &[p], PositionScope::Playlist(pid))?;
        tx.commit()?;
        Ok(())
    }
    pub fn move_playlist_entry(&self, id: i64, index: usize) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let (pid, old): (i64, i64) = tx
            .query_row(
                "SELECT playlist_id,position FROM playlist_entries WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("playlist entry {id} does not exist"))?;
        let n: i64 = tx.query_row(
            "SELECT COUNT(*)FROM playlist_entries WHERE playlist_id=?",
            [pid],
            |r| r.get(0),
        )?;
        if index >= n as usize {
            bail!("playlist index out of range");
        }
        move_entry(&tx, PositionScope::Playlist(pid), id, old, index as i64)?;
        tx.commit()?;
        Ok(())
    }
}

fn append_tracks(tx: &Transaction<'_>, id: i64, tracks: &[i64]) -> Result<()> {
    let tracks = serde_json::to_string(tracks)?;
    validate_encoded_track_ids(tx, &tracks)?;
    tx.execute("WITH selected AS (SELECT value AS track_id,MIN(CAST(key AS INTEGER)) AS ordinal FROM json_each(?2) GROUP BY value), additions AS (SELECT track_id,ordinal FROM selected WHERE NOT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=?1 AND playlist_entries.track_id=selected.track_id)) INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?1,track_id,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1)+row_number() OVER (ORDER BY ordinal)-1 FROM additions ORDER BY ordinal", params![id, tracks])?;
    Ok(())
}

fn exists_playlist(tx: &Transaction<'_>, id: i64) -> Result<()> {
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?)",
        [id],
        |r| r.get(0),
    )? {
        bail!("playlist {id} does not exist");
    }
    Ok(())
}

fn valid_name(s: &str) -> Result<()> {
    if s.trim().is_empty() || s.chars().any(char::is_control) {
        bail!("invalid name")
    }
    Ok(())
}

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn playlist_entry_location_tracks_reorder_and_deletion_by_identity() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES (1,'one'),(2,'two'),(3,'three');
             INSERT INTO playlists(id,name) VALUES (10,'first'),(20,'second');
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES
                (100,10,1,0),(101,10,2,1),(102,10,3,2),(200,20,1,0);",
        )?;
        let expected = PlaylistEntryLocation {
            id: 102,
            playlist_id: 10,
            track_id: 3,
            position: 2,
            total: 3,
        };
        assert_eq!(store.playlist_entry(102)?, Some(expected));
        store.move_playlist_entry(102, 0)?;
        assert_eq!(
            store.playlist_entry(102)?,
            Some(PlaylistEntryLocation {
                position: 0,
                ..expected
            })
        );
        store.remove_playlist_entry(100)?;
        assert_eq!(
            store.playlist_entry(102)?,
            Some(PlaylistEntryLocation {
                position: 0,
                total: 2,
                ..expected
            })
        );
        assert_eq!(store.playlist_entry(200)?.unwrap().total, 1);
        store.remove_playlist_entry(102)?;
        assert_eq!(store.playlist_entry(102)?, None);
        assert_eq!(store.playlist_entry(999)?, None);
        store.delete_playlist(20)?;
        assert_eq!(store.playlist_entry(200)?, None);
        Ok(())
    }

    #[test]
    fn remove_playlist_entry_preserves_identity_and_other_references() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES (1,'one'),(2,'two'),(3,'three');
             INSERT INTO playlists(id,name) VALUES (10,'first'),(20,'second');
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES
                (100,10,1,0),(101,10,2,1),(102,10,3,2),
                (200,20,1,0),(201,20,2,1),(202,20,3,2);
             INSERT INTO queue_entries(id,track_id,position) VALUES (300,2,0);
             UPDATE queue_meta SET current_id=300 WHERE singleton=1;",
        )?;
        store.remove_playlist_entry(101)?;
        assert!(store.remove_playlist_entry(999).is_err());
        let entries = store.conn.prepare(
            "SELECT id,playlist_id,track_id,position FROM playlist_entries ORDER BY playlist_id,position",
        )?.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?,row.get::<_, i64>(1)?,row.get::<_, i64>(2)?,row.get::<_, i64>(3)?))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(
            entries,
            vec![
                (100, 10, 1, 0),
                (102, 10, 3, 1),
                (200, 20, 1, 0),
                (201, 20, 2, 1),
                (202, 20, 3, 2),
            ]
        );
        let current: Option<i64> = store.conn.query_row(
            "SELECT current_id FROM queue_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(current, Some(300));
        let tracks: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))?;
        assert_eq!(tracks, 3);
        Ok(())
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use std::path::Path;

    fn store() -> Result<Store> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES (1,'one'),(2,'two'),(3,'three'),(4,'four');",
        )?;
        Ok(store)
    }

    #[test]
    fn rename_reports_persistent_changes_and_rejects_unknown_ids() -> Result<()> {
        let store = store()?;
        let id = store.create_playlist_with_tracks("Original", &[3, 1])?;
        let entries = store.playlist_entries_page(id, 0, 10)?.rows;
        assert!(!store.rename_playlist(id, "Original")?);
        assert!(store.rename_playlist(id, "Renamed")?);
        assert!(!store.rename_playlist(id, "Renamed")?);
        assert!(store.rename_playlist(i64::MAX, "Renamed").is_err());
        assert_eq!(store.playlist_entries_page(id, 0, 10)?.rows, entries);
        let name: String =
            store
                .conn
                .query_row("SELECT name FROM playlists WHERE id=?1", [id], |row| {
                    row.get(0)
                })?;
        assert_eq!(name, "Renamed");
        Ok(())
    }

    #[test]
    fn append_keeps_first_occurrence_order_and_invalid_ids_roll_back() -> Result<()> {
        let store = store()?;
        assert!(
            store
                .create_playlist_with_tracks("invalid", &[1, -1, 2])
                .is_err()
        );
        assert_eq!(store.playlist_summary_page(0, 0)?.total, 0);
        let id = store.create_playlist_with_tracks("valid", &[3, 1, 3, 2, 1])?;
        let original = store.playlist_entries_page(id, 0, 10)?.rows;
        assert_eq!(store.playlist_track_ids(id, 10)?, vec![3, 1, 2]);
        assert!(store.add_playlist(id, &[4, i64::MAX, 4]).is_err());
        assert!(
            store
                .add_playlist_sources(id, &[PathBuf::new()], &[i64::MAX])
                .is_err()
        );
        assert_eq!(store.playlist_track_ids(id, 10)?, vec![3, 1, 2]);
        assert_eq!(
            store
                .playlist_entries_page(id, 0, 10)?
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            original.iter().map(|row| row.id).collect::<Vec<_>>()
        );
        store.add_playlist(id, &[2, 4, 3, 4])?;
        assert_eq!(store.playlist_track_ids(id, 10)?, vec![3, 1, 2, 4]);
        Ok(())
    }

    #[test]
    fn count_only_pages_keep_totals_and_unknown_playlist_errors() -> Result<()> {
        let store = store()?;
        let id = store.create_playlist_with_tracks("entries", &[1, 2, 3])?;
        store.create_playlist("empty")?;
        let summaries = store.playlist_summary_page(usize::MAX, 0)?;
        assert_eq!(summaries.total, 2);
        assert!(summaries.rows.is_empty());
        let entries = store.playlist_entries_page(id, usize::MAX, 0)?;
        assert_eq!(entries.total, 3);
        assert!(entries.rows.is_empty());
        assert!(store.playlist_entries_page(i64::MAX, 0, 0).is_err());
        assert_eq!(store.playlist_entries_page(id, 1, 1)?.rows[0].track_id, 2);
        assert_eq!(
            store.playlist_entries_page(id, 0, usize::MAX)?.rows.len(),
            3
        );
        Ok(())
    }

    fn positioned_entries(store: &Store, id: i64) -> Result<Vec<(i64, i64, i64)>> {
        Ok(store.conn.prepare(
            "SELECT id,track_id,position FROM playlist_entries WHERE playlist_id=?1 ORDER BY position,id",
        )?.query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[test]
    fn moves_preserve_identity_and_no_op_or_invalid_moves_preserve_order() -> Result<()> {
        let store = store()?;
        let id = store.create_playlist_with_tracks("moves", &[1, 2, 3, 4])?;
        let other = store.create_playlist_with_tracks("other", &[4, 3])?;
        let other_before = positioned_entries(&store, other)?;
        let initial = positioned_entries(&store, id)?;
        store.move_playlist_entry(initial[1].0, 3)?;
        assert_eq!(
            positioned_entries(&store, id)?,
            vec![
                initial[0],
                (initial[2].0, 3, 1),
                (initial[3].0, 4, 2),
                (initial[1].0, 2, 3)
            ]
        );
        store.move_playlist_entry(initial[1].0, 0)?;
        let moved = positioned_entries(&store, id)?;
        assert_eq!(
            moved,
            vec![
                (initial[1].0, 2, 0),
                (initial[0].0, 1, 1),
                (initial[2].0, 3, 2),
                (initial[3].0, 4, 3)
            ]
        );
        store.move_playlist_entry(initial[1].0, 0)?;
        assert!(store.move_playlist_entry(initial[1].0, 4).is_err());
        assert!(store.move_playlist_entry(i64::MAX, 0).is_err());
        assert_eq!(positioned_entries(&store, id)?, moved);
        assert_eq!(positioned_entries(&store, other)?, other_before);
        Ok(())
    }
}
