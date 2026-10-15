use super::positions::{PositionScope, close_gaps, move_entry};
use super::rows::{TRACK_COLUMNS, track_from_row};
use super::tracks::{SOURCE_FILTER, validate_encoded_track_ids};
use super::{Store, batch_limit, changed};
use crate::model::{
    Batch, Page, PlaylistEntryCursor, PlaylistEntryLocation, PlaylistEntryRow, PlaylistSummary,
    Track,
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
    pub fn playlist_entry_locations(
        &self,
        playlist_id: i64,
        entry_ids: &[i64],
    ) -> Result<Vec<PlaylistEntryLocation>> {
        anyhow::ensure!(
            entry_ids.len() <= crate::model::MAX_QUERY_ROWS,
            "Playlist entry query exceeds {} IDs",
            crate::model::MAX_QUERY_ROWS
        );
        let total: i64 = self.conn.query_row(
            "SELECT (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1",
            [playlist_id],
            |row| row.get(0),
        ).optional()?.context("Playlist not found")?;
        let ids = serde_json::to_string(entry_ids)?;
        let mut statement = self.conn.prepare(
            "SELECT id,track_id,position FROM playlist_entries
             WHERE playlist_id=?1 AND id IN (SELECT value FROM json_each(?2))
             ORDER BY position,id",
        )?;
        Ok(statement
            .query_map(params![playlist_id, ids], |row| {
                Ok(PlaylistEntryLocation {
                    id: row.get(0)?,
                    playlist_id,
                    track_id: row.get(1)?,
                    position: row.get::<_, i64>(2)? as usize,
                    total: total as usize,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
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
            "SELECT p.id,p.name,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries e WHERE e.playlist_id=p.id)
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
    ) -> Result<bool> {
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
        let inserted = tx.execute(&format!(
            "WITH selected AS (SELECT id FROM tracks WHERE ({SOURCE_FILTER}) AND NOT EXISTS (SELECT 1 FROM playlist_entries e WHERE e.playlist_id=?3 AND e.track_id=tracks.id)), ordered AS (SELECT id,row_number() OVER (ORDER BY id)-1 AS ordinal FROM selected) INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?3,id,(SELECT COALESCE(MAX(position),-1)+1 FROM playlist_entries WHERE playlist_id=?3)+ordinal FROM ordered"
        ), params![directories, ids, playlist_id])?;
        tx.commit()?;
        Ok(inserted != 0)
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
                        (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries e WHERE e.playlist_id=p.id)
                 FROM playlists p WHERE p.id>?1 ORDER BY p.id LIMIT ?2",
            )?
        } else {
            self.conn.prepare(
                "SELECT p.id,p.name,
                        (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries e WHERE e.playlist_id=p.id)
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
                        COALESCE(t.missing,1),t.favorite
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
                        COALESCE(t.missing,1),t.favorite
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
                favorite: row.get(7)?,
            });
            last_cursor = Some(cursor);
        }
        let next = has_more.then_some(last_cursor).flatten();
        Ok(Batch { rows, next })
    }

    pub(crate) fn playlist_track_batch(
        &self,
        playlist_id: i64,
        after: Option<PlaylistEntryCursor>,
        limit: usize,
    ) -> Result<Batch<Track, PlaylistEntryCursor>> {
        let limit = batch_limit(limit)?;
        let exists = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
            [playlist_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            bail!("Playlist not found");
        }
        // The derived table reuses the full Track projection and decoder. A LEFT JOIN
        // keeps orphan entries visible so export fails rather than skipping them.
        let suffix = if after.is_some() {
            " AND (e.position,e.id)>(?2,?3) ORDER BY e.position,e.id LIMIT ?4"
        } else {
            " ORDER BY e.position,e.id LIMIT ?2"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT t.*,e.position AS entry_position,e.id AS entry_id
             FROM playlist_entries e LEFT JOIN (SELECT {TRACK_COLUMNS} FROM tracks) t
             ON t.id=e.track_id WHERE e.playlist_id=?1{suffix}"
        ))?;
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
            row.get::<_, Option<i64>>(0)?
                .context("Playlist contains a missing library track")?;
            rows.push(track_from_row(row)?);
            last_cursor = Some(PlaylistEntryCursor {
                position: row.get("entry_position")?,
                id: row.get("entry_id")?,
            });
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
            "SELECT (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1",
            [playlist_id],
            |r| r.get(0),
        ).optional()?.context("Playlist not found")?;
        if limit == 0 {
            return Ok(Page {
                total: total as usize,
                rows: Vec::new(),
            });
        }
        let mut stmt = self.conn.prepare("SELECT e.id,e.track_id,COALESCE(t.title_override,t.raw_title),COALESCE(t.artist_override,t.raw_artist),COALESCE(t.album_override,t.raw_album),COALESCE(t.missing,1),t.favorite FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id WHERE e.playlist_id=?1 AND e.position>=?3 ORDER BY e.position,e.id LIMIT ?2")?;
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
                    favorite: r.get(6)?,
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

    pub(crate) fn validate_playlist_name(name: &str) -> Result<()> {
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            bail!("invalid name")
        }
        Ok(())
    }

    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        self.create_playlist_with_tracks(name, &[])
    }

    pub fn create_playlist_with_tracks(&self, name: &str, tracks: &[i64]) -> Result<i64> {
        Self::validate_playlist_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO playlists(name)VALUES(?)", [name])?;
        let id = tx.last_insert_rowid();
        append_tracks(&tx, id, tracks)?;
        tx.commit()?;
        Ok(id)
    }
    pub fn create_playlist_from_queue(&self, name: &str) -> Result<i64> {
        Self::validate_playlist_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO playlists(name) VALUES(?1)", [name])?;
        let id = tx.last_insert_rowid();
        tx.execute("WITH first AS (SELECT track_id,MIN(position) AS position FROM queue_entries GROUP BY track_id) INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?1,track_id,row_number() OVER (ORDER BY position,track_id)-1 FROM first ORDER BY position,track_id", [id])?;
        tx.commit()?;
        Ok(id)
    }
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<bool> {
        Self::validate_playlist_name(name)?;
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
    pub fn add_playlist(&self, id: i64, tracks: &[i64]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        exists_playlist(&tx, id)?;
        let inserted = append_tracks(&tx, id, tracks)?;
        tx.commit()?;
        Ok(inserted != 0)
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
    /// Returns the owning playlist only after an actual committed reorder.
    pub fn move_playlist_entry(&self, id: i64, index: usize) -> Result<Option<i64>> {
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
            "SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?",
            [pid],
            |r| r.get(0),
        )?;
        if index >= n as usize {
            bail!("playlist index out of range");
        }
        if old == index as i64 {
            return Ok(None);
        }
        move_entry(&tx, PositionScope::Playlist(pid), id, old, index as i64)?;
        tx.commit()?;
        Ok(Some(pid))
    }

    pub fn remove_playlist_entries(&self, playlist_id: i64, entry_ids: &[i64]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let ids = serde_json::to_string(entry_ids)?;
        let selected = selected_entries(&tx, playlist_id, &ids)?;
        if selected.is_empty() {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM playlist_entries WHERE playlist_id=?1 AND id IN (SELECT value FROM json_each(?2))",
            params![playlist_id, ids],
        )?;
        close_gaps(&tx, &selected, PositionScope::Playlist(playlist_id))?;
        tx.commit()?;
        Ok(true)
    }

    /// Move stable occurrences as a group in their original playlist order.
    /// Rust retains only selected positions, never the whole playlist or metadata.
    pub fn move_playlist_entries(
        &self,
        playlist_id: i64,
        entry_ids: &[i64],
        before_entry_id: Option<i64>,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let ids = serde_json::to_string(entry_ids)?;
        let selected = selected_entries(&tx, playlist_id, &ids)?;
        let target: i64 = match before_entry_id {
            Some(id) => tx
                .query_row(
                    "SELECT position FROM playlist_entries WHERE playlist_id=?1 AND id=?2",
                    params![playlist_id, id],
                    |row| row.get(0),
                )
                .optional()?
                .context("Playlist target entry does not exist in this playlist")?,
            None => tx.query_row(
                "SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1",
                [playlist_id],
                |row| row.get(0),
            )?,
        };
        if selected.is_empty() || selected.binary_search(&target).is_ok() {
            return Ok(false);
        }
        let count = i64::try_from(selected.len())?;
        let insertion = target - selected.partition_point(|&position| position < target) as i64;
        if selected
            .iter()
            .enumerate()
            .all(|(ordinal, &position)| position == insertion + ordinal as i64)
        {
            return Ok(false);
        }
        if let [old] = selected.as_slice() {
            // Validation collapsed duplicate IDs to this one stable occurrence.
            // Shift only the intervening interval, not the unchanged suffix twice.
            move_entry(
                &tx,
                PositionScope::Playlist(playlist_id),
                entry_ids[0],
                *old,
                insertion,
            )?;
            tx.commit()?;
            return Ok(true);
        }
        // Temporarily park selected rows outside the nonnegative position domain.
        // The shared gap closer then touches survivors only, once per original interval.
        tx.execute(
            "WITH selected AS MATERIALIZED (
                SELECT id,-row_number() OVER (ORDER BY position,id) AS parked
                FROM playlist_entries WHERE playlist_id=?1
                AND id IN (SELECT value FROM json_each(?2))
             ) UPDATE playlist_entries SET position=selected.parked
               FROM selected WHERE playlist_entries.id=selected.id",
            params![playlist_id, ids],
        )?;
        close_gaps(&tx, &selected, PositionScope::Playlist(playlist_id))?;
        tx.execute(
            "UPDATE playlist_entries SET position=position+?1 WHERE playlist_id=?2 AND position>=?3",
            params![count, playlist_id, insertion],
        )?;
        tx.execute(
            "UPDATE playlist_entries SET position=?1-position-1 WHERE playlist_id=?2 AND position<0",
            params![insertion, playlist_id],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

fn selected_entries(tx: &Transaction<'_>, playlist_id: i64, ids: &str) -> Result<Vec<i64>> {
    exists_playlist(tx, playlist_id)?;
    let invalid: Option<i64> = tx
        .query_row(
            "SELECT value FROM json_each(?2) WHERE NOT EXISTS
         (SELECT 1 FROM playlist_entries WHERE playlist_id=?1 AND id=value) LIMIT 1",
            params![playlist_id, ids],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = invalid {
        bail!("playlist entry {id} does not exist in playlist {playlist_id}");
    }
    let mut statement = tx.prepare(
        "SELECT position FROM playlist_entries WHERE playlist_id=?1
         AND id IN (SELECT value FROM json_each(?2)) ORDER BY position,id",
    )?;
    Ok(statement
        .query_map(params![playlist_id, ids], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn append_tracks(tx: &Transaction<'_>, id: i64, tracks: &[i64]) -> Result<usize> {
    let tracks = serde_json::to_string(tracks)?;
    validate_encoded_track_ids(tx, &tracks)?;
    Ok(tx.execute("WITH selected AS (SELECT value AS track_id,MIN(CAST(key AS INTEGER)) AS ordinal FROM json_each(?2) GROUP BY value), additions AS (SELECT track_id,ordinal FROM selected WHERE NOT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=?1 AND playlist_entries.track_id=selected.track_id)) INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?1,track_id,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1)+row_number() OVER (ORDER BY ordinal)-1 FROM additions ORDER BY ordinal", params![id, tracks])?)
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
        assert_eq!(store.move_playlist_entry(102, 0)?, Some(10));
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
    fn create_and_rename_share_name_rules_without_normalizing_whitespace() -> Result<()> {
        let store = store()?;
        let original = store.create_playlist_with_tracks("Original", &[3, 1])?;
        let before = store.playlist_summary_page(0, 10)?;
        let entries = store.playlist_entries_page(original, 0, 10)?;
        for name in [
            "",
            " ",
            "\u{2003}",
            "\tMix",
            "Mix\n",
            "Mix\u{7f}",
            "Mix\u{85}",
        ] {
            assert!(store.create_playlist(name).is_err());
            assert!(store.create_playlist_with_tracks(name, &[1]).is_err());
            assert!(store.create_playlist_from_queue(name).is_err());
            assert!(store.rename_playlist(original, name).is_err());
            assert_eq!(store.playlist_summary_page(0, 10)?, before);
            assert_eq!(store.playlist_entries_page(original, 0, 10)?, entries);
        }
        for name in ["  Mix \u{2003} ", "混音", "Mix\u{200b}"] {
            let plain = store.create_playlist(name)?;
            let with_tracks = store.create_playlist_with_tracks(name, &[1])?;
            let from_queue = store.create_playlist_from_queue(name)?;
            assert!(store.rename_playlist(original, name)?);
            let summaries = store.playlist_summary_page(0, 100)?.rows;
            for id in [plain, with_tracks, from_queue, original] {
                assert_eq!(
                    summaries.iter().find(|row| row.id == id).unwrap().name,
                    name
                );
            }
        }
        Ok(())
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
    fn appends_report_changes_without_skipping_validation() -> Result<()> {
        let store = store()?;
        let id = store.create_playlist_with_tracks("append", &[1, 2])?;
        let before = store.playlist_entries_page(id, 0, 10)?.rows;
        assert!(!store.add_playlist(id, &[])?);
        assert!(!store.add_playlist(id, &[2, 1, 2])?);
        assert!(!store.add_playlist_sources(id, &[], &[])?);
        assert!(!store.add_playlist_sources(id, &[], &[1, 2])?);
        assert!(store.add_playlist(i64::MAX, &[]).is_err());
        assert!(store.add_playlist_sources(i64::MAX, &[], &[]).is_err());
        assert!(store.add_playlist(id, &[1, i64::MAX]).is_err());
        assert!(store.add_playlist_sources(id, &[], &[1, i64::MAX]).is_err());
        assert_eq!(store.playlist_entries_page(id, 0, 10)?.rows, before);
        assert!(store.add_playlist(id, &[2, 3, 3])?);
        assert!(store.add_playlist_sources(id, &[], &[3, 4])?);
        assert_eq!(store.playlist_track_ids(id, 10)?, [1, 2, 3, 4]);
        Ok(())
    }

    #[test]
    fn deep_pages_preserve_entry_identity_missing_tracks_and_exact_totals() -> Result<()> {
        let store = store()?;
        let total = crate::model::MAX_QUERY_ROWS + 17;
        store.conn.execute(
            "WITH RECURSIVE ids(id) AS (SELECT 5 UNION ALL SELECT id+1 FROM ids WHERE id<?1) INSERT INTO tracks(id,path) SELECT id,'track-'||id FROM ids",
            [total as i64],
        )?;
        let tracks = (1..=total as i64).collect::<Vec<_>>();
        let id = store.create_playlist_with_tracks("deep", &tracks)?;
        let tail = store.playlist_entries_page(id, total - 3, usize::MAX)?;
        assert_eq!(tail.total, total);
        assert_eq!(
            tail.rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
            tracks[total - 3..]
        );
        store.conn.execute(
            "UPDATE tracks SET missing=1 WHERE id=?1",
            [total as i64 - 1],
        )?;
        let page = store.playlist_entries_page(id, total - 3, 2)?;
        assert_eq!(page.total, total);
        assert_eq!(page.rows[0], tail.rows[0]);
        assert_eq!(page.rows[1].id, tail.rows[1].id);
        assert_eq!(page.rows[1].track_id, tail.rows[1].track_id);
        assert!(page.rows[1].missing);
        assert_eq!(page.rows[1].title, tail.rows[1].title);
        for offset in [total, total + 1, usize::MAX] {
            let page = store.playlist_entries_page(id, offset, 3)?;
            assert_eq!(page.total, total);
            assert!(page.rows.is_empty());
        }
        assert_eq!(store.playlist_entries_page(id, usize::MAX, 0)?.total, total);
        assert_eq!(
            store.playlist_entries_page(id, 0, usize::MAX)?.rows.len(),
            crate::model::MAX_QUERY_ROWS
        );
        assert_eq!(
            store.playlist_summary_page(0, 10)?.rows[0].entry_count,
            total
        );
        assert_eq!(
            store.playlist_summary_batch(None, 10)?.rows[0].entry_count,
            total
        );
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
        assert_eq!(store.move_playlist_entry(initial[1].0, 3)?, Some(id));
        assert_eq!(
            positioned_entries(&store, id)?,
            vec![
                initial[0],
                (initial[2].0, 3, 1),
                (initial[3].0, 4, 2),
                (initial[1].0, 2, 3)
            ]
        );
        assert_eq!(store.move_playlist_entry(initial[1].0, 0)?, Some(id));
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
        assert_eq!(store.move_playlist_entry(initial[1].0, 0)?, None);
        assert!(store.move_playlist_entry(initial[1].0, 4).is_err());
        assert!(store.move_playlist_entry(initial[1].0, usize::MAX).is_err());
        assert!(store.move_playlist_entry(i64::MAX, 0).is_err());
        assert_eq!(positioned_entries(&store, id)?, moved);
        assert_eq!(positioned_entries(&store, other)?, other_before);
        Ok(())
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use crate::model::MAX_QUERY_ROWS;
    use std::path::Path;

    #[test]
    fn track_batches_preserve_order_across_the_row_bound() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let total = MAX_QUERY_ROWS + 3;
        store.conn.execute(
            "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<?1)
             INSERT INTO tracks(id,path) SELECT id,'track-'||id FROM ids",
            [total as i64],
        )?;
        store
            .conn
            .execute("INSERT INTO playlists(id,name) VALUES(1,'ordered')", [])?;
        store.conn.execute(
            "INSERT INTO playlist_entries(id,playlist_id,track_id,position)
             SELECT ?1-id+1000,1,id,?1-id FROM tracks",
            [total as i64],
        )?;
        let first = store.playlist_track_batch(1, None, MAX_QUERY_ROWS)?;
        assert_eq!(first.rows.len(), MAX_QUERY_ROWS);
        assert_eq!(
            first.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
            (4..=total as i64).rev().collect::<Vec<_>>()
        );
        assert_eq!(
            first.next,
            Some(PlaylistEntryCursor {
                position: MAX_QUERY_ROWS as i64 - 1,
                id: MAX_QUERY_ROWS as i64 + 999,
            })
        );
        let tail = store.playlist_track_batch(1, first.next, MAX_QUERY_ROWS)?;
        assert_eq!(
            tail.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
            [3, 2, 1]
        );
        assert!(tail.next.is_none());
        // A full final batch must terminate without an extra empty-page fetch.
        let exact_tail = store.playlist_track_batch(1, first.next, 3)?;
        assert_eq!(exact_tail.rows, tail.rows);
        assert!(exact_tail.next.is_none());
        let exhausted = store.playlist_track_batch(
            1,
            Some(PlaylistEntryCursor {
                position: total as i64 - 1,
                id: total as i64 + 999,
            }),
            MAX_QUERY_ROWS,
        )?;
        assert!(exhausted.rows.is_empty());
        assert!(exhausted.next.is_none());
        Ok(())
    }

    #[test]
    fn track_batches_distinguish_empty_unknown_and_orphaned_playlists() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let empty = store.create_playlist("empty")?;
        let batch = store.playlist_track_batch(empty, None, 1)?;
        assert!(batch.rows.is_empty());
        assert!(batch.next.is_none());
        assert!(store.playlist_track_batch(i64::MAX, None, 1).is_err());
        assert!(store.playlist_track_batch(empty, None, 0).is_err());
        assert!(
            store
                .playlist_track_batch(empty, None, MAX_QUERY_ROWS + 1)
                .is_err()
        );
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES(1,'one'),(3,'three');
             INSERT INTO playlist_entries(id,playlist_id,track_id,position)
             VALUES(10,1,1,0),(20,1,2,1),(30,1,3,2);",
        )?;
        // An orphan in the lookahead must produce a cursor, not a false end of stream.
        let first = store.playlist_track_batch(empty, None, 1)?;
        assert_eq!(first.rows[0].id, 1);
        assert!(first.next.is_some());
        for (after, limit) in [(None, 3), (first.next, 1)] {
            assert!(store.playlist_track_batch(empty, after, limit).is_err());
        }
        Ok(())
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use std::path::Path;

    fn fixture() -> Result<Store> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES (1,'one'),(2,'two'),(3,'three'),(4,'four'),(5,'five'),(6,'six');
             INSERT INTO playlists(id,name) VALUES (10,'first'),(20,'second');
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES
             (100,10,1,0),(101,10,2,1),(102,10,3,2),(103,10,4,3),(104,10,5,4),(105,10,6,5),(200,20,1,0);",
        )?;
        Ok(store)
    }

    fn order(store: &Store, playlist_id: i64) -> Result<Vec<i64>> {
        let mut statement = store.conn.prepare(
            "SELECT id,position FROM playlist_entries WHERE playlist_id=?1 ORDER BY position,id",
        )?;
        let rows = statement
            .query_map([playlist_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (index, &(_, position)) in rows.iter().enumerate() {
            assert_eq!(position, index as i64);
        }
        Ok(rows.into_iter().map(|(id, _)| id).collect())
    }

    #[test]
    fn noncontiguous_groups_move_before_after_and_to_end_in_source_order() -> Result<()> {
        for (target, expected) in [
            (Some(100), vec![101, 103, 100, 102, 104, 105]),
            (Some(102), vec![100, 101, 103, 102, 104, 105]),
            (Some(105), vec![100, 102, 104, 101, 103, 105]),
            (None, vec![100, 102, 104, 105, 101, 103]),
        ] {
            let store = fixture()?;
            assert!(store.move_playlist_entries(10, &[103, 101, 103], target)?);
            assert_eq!(order(&store, 10)?, expected);
            assert_eq!(order(&store, 20)?, [200]);
        }
        Ok(())
    }

    #[test]
    fn singleton_groups_preserve_identity_in_both_directions_and_at_end() -> Result<()> {
        for (id, target, expected) in [
            (100, Some(102), vec![101, 100, 102, 103, 104, 105]),
            (103, Some(100), vec![103, 100, 101, 102, 104, 105]),
            (101, None, vec![100, 102, 103, 104, 105, 101]),
        ] {
            let store = fixture()?;
            assert!(store.move_playlist_entries(10, &[id, id], target)?);
            assert_eq!(order(&store, 10)?, expected);
            assert_eq!(order(&store, 20)?, [200]);
        }
        Ok(())
    }

    #[test]
    fn group_noops_validate_every_identity_and_target() -> Result<()> {
        let store = fixture()?;
        let before = order(&store, 10)?;
        for (ids, target) in [
            (vec![], None),
            (vec![101, 102], Some(103)),
            (vec![105], None),
            (vec![101, 103], Some(103)),
            (vec![100, 101, 102, 103, 104, 105], None),
        ] {
            assert!(!store.move_playlist_entries(10, &ids, target)?);
            assert_eq!(order(&store, 10)?, before);
        }
        assert!(!store.remove_playlist_entries(10, &[])?);
        for (playlist, ids, target) in [
            (999, vec![], None),
            (10, vec![101, 999], Some(101)),
            (10, vec![101, 200], Some(101)),
            (10, vec![], Some(999)),
            (10, vec![101], Some(200)),
        ] {
            assert!(store.move_playlist_entries(playlist, &ids, target).is_err());
            assert_eq!(order(&store, 10)?, before);
            assert_eq!(order(&store, 20)?, [200]);
        }
        for (playlist, ids) in [(999, vec![]), (10, vec![101, 999]), (10, vec![101, 200])] {
            assert!(store.remove_playlist_entries(playlist, &ids).is_err());
            assert_eq!(order(&store, 10)?, before);
        }
        Ok(())
    }

    #[test]
    fn group_removal_closes_gaps_once_and_preserves_other_references() -> Result<()> {
        let store = fixture()?;
        store
            .conn
            .execute_batch("INSERT INTO queue_entries(id,track_id,position) VALUES (300,2,0);")?;
        assert!(store.remove_playlist_entries(10, &[103, 101, 103])?);
        assert_eq!(order(&store, 10)?, [100, 102, 104, 105]);
        assert_eq!(order(&store, 20)?, [200]);
        assert_eq!(
            store.conn.query_row(
                "SELECT track_id FROM queue_entries WHERE id=300",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            2
        );
        assert!(store.track(2)?.is_some());
        Ok(())
    }

    #[test]
    fn group_mutation_failure_rolls_back_intermediate_positions_and_deletes() -> Result<()> {
        let store = fixture()?;
        let before = order(&store, 10)?;
        store.conn.execute_batch(
            "CREATE TRIGGER reject_restore BEFORE UPDATE OF position ON playlist_entries
             WHEN OLD.position<0 AND NEW.position>=0 BEGIN SELECT RAISE(ABORT,'reject restore'); END;",
        )?;
        assert!(store.move_playlist_entries(10, &[101, 103], None).is_err());
        assert_eq!(order(&store, 10)?, before);
        store.conn.execute_batch(
            "DROP TRIGGER reject_restore;
             CREATE TRIGGER reject_gap BEFORE UPDATE OF position ON playlist_entries
             BEGIN SELECT RAISE(ABORT,'reject gap'); END;",
        )?;
        assert!(store.remove_playlist_entries(10, &[101, 103]).is_err());
        assert_eq!(order(&store, 10)?, before);
        Ok(())
    }

    #[test]
    fn singleton_group_failure_rolls_back_the_shifted_interval() -> Result<()> {
        let store = fixture()?;
        let before = order(&store, 10)?;
        store.conn.execute_batch(
            "CREATE TRIGGER reject_selected BEFORE UPDATE OF position ON playlist_entries
             WHEN OLD.id=100 BEGIN SELECT RAISE(ABORT,'reject selected'); END;",
        )?;
        assert!(
            store
                .move_playlist_entries(10, &[100, 100], Some(103))
                .is_err()
        );
        assert_eq!(order(&store, 10)?, before);
        assert_eq!(order(&store, 20)?, [200]);
        Ok(())
    }

    #[test]
    fn compact_locations_are_bounded_scoped_and_survive_reordering() -> Result<()> {
        let store = fixture()?;
        store.conn.execute_batch(
            "INSERT INTO playlists(id,name) VALUES (30,'large');
             WITH RECURSIVE entries(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM entries WHERE n<1000)
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) SELECT 1000+n,30,1000+n,n-1 FROM entries;",
        )?;
        let ids: Vec<_> = (1001..=1000 + crate::model::MAX_QUERY_ROWS as i64).collect();
        let locations = store.playlist_entry_locations(30, &ids)?;
        assert_eq!(locations.len(), crate::model::MAX_QUERY_ROWS);
        assert!(
            locations
                .iter()
                .enumerate()
                .all(|(index, row)| row.position == index && row.total == 1000)
        );
        assert!(
            store
                .playlist_entry_locations(30, &vec![1001; crate::model::MAX_QUERY_ROWS + 1])
                .is_err()
        );
        assert!(store.playlist_entry_locations(999, &[]).is_err());
        assert!(store.playlist_entry_locations(30, &[])?.is_empty());
        assert_eq!(
            store
                .playlist_entry_locations(10, &[103, 200, 999, 103])?
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [103]
        );
        store.move_playlist_entries(10, &[103], Some(100))?;
        let row = store.playlist_entry_locations(10, &[103])?.remove(0);
        assert_eq!(
            (row.id, row.track_id, row.position, row.total),
            (103, 4, 0, 6)
        );
        Ok(())
    }
}
