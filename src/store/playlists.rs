use super::tracks::SOURCE_FILTER;
use super::{Store, batch_limit, changed};
use crate::model::{Batch, Page, PlaylistEntryCursor, PlaylistEntryRow, PlaylistSummary};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, Transaction, params};
use std::collections::HashSet;
use std::path::PathBuf;

impl Store {
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
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<()> {
        valid_name(name)?;
        changed(
            self.conn
                .execute("UPDATE playlists SET name=? WHERE id=?", params![name, id])?,
            "playlist",
            id,
        )
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
        tx.execute(
            "UPDATE playlist_entries SET position=position-1 WHERE playlist_id=? AND position>?",
            params![pid, p],
        )?;
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
        let new = index as i64;
        if new < old {
            tx.execute("UPDATE playlist_entries SET position=position+1 WHERE playlist_id=? AND position>=? AND position<?",params![pid,new,old])?;
        } else if new > old {
            tx.execute("UPDATE playlist_entries SET position=position-1 WHERE playlist_id=? AND position>? AND position<=?",params![pid,old,new])?;
        }
        tx.execute(
            "UPDATE playlist_entries SET position=? WHERE id=?",
            params![new, id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn append_tracks(tx: &Transaction<'_>, id: i64, tracks: &[i64]) -> Result<()> {
    let mut position: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?",
        [id],
        |r| r.get(0),
    )?;
    let mut seen = HashSet::with_capacity(tracks.len());
    let mut exists = tx.prepare("SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)")?;
    let mut insert = tx.prepare(
        "INSERT INTO playlist_entries(playlist_id,track_id,position)
         SELECT ?1,?2,?3 WHERE NOT EXISTS(
             SELECT 1 FROM playlist_entries WHERE playlist_id=?1 AND track_id=?2
         )",
    )?;
    for &track_id in tracks {
        if !seen.insert(track_id) {
            continue;
        }
        if !exists.query_row([track_id], |row| row.get::<_, bool>(0))? {
            bail!("Track {track_id} does not exist");
        }
        position += insert.execute(params![id, track_id, position])? as i64;
    }
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
