use super::positions::{close_gaps, move_selection};
use super::rows::cue_from_row;
use super::tracks::{SOURCE_FILTER, search_filter, search_query, validate_encoded_track_ids};
use super::{Store, batch_limit, changed};
use anyhow::{Context, Result, anyhow, bail};
use rivu_protocol::model::{
    Batch, BatchContinuation, Page, PlaylistEntryCursor, PlaylistEntryLocation, PlaylistEntryRow,
    PlaylistExportTrack, PlaylistSummary, QueryStamp,
};
use rusqlite::{OptionalExtension, Transaction, params};
use std::path::PathBuf;

impl Store {
    pub fn playlist_entry(&self, entry_id: i64) -> Result<Option<PlaylistEntryLocation>> {
        // Positions are contiguous. The covering playlist_order index supplies the last
        // position without counting or projecting the entire playlist.
        Ok(self.conn.query_row(
            "SELECT e.id,e.playlist_id,e.track_id,e.position,
                    (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=e.playlist_id)
             FROM playlist_entries e WHERE e.id=?1 AND e.playlist_id>0",
            [entry_id],
            |row| Ok(PlaylistEntryLocation {
                id: row.get(0)?,
                playlist_id: row.get(1)?,
                track_id: row.get(2)?,
                position: row.get::<_, i64>(3)? as usize,
                offset: row.get::<_, i64>(3)? as usize,
                total: row.get::<_, i64>(4)? as usize,
            }),
        ).optional()?)
    }
    pub fn playlist_entry_locations(
        &self,
        playlist_id: i64,
        query: Option<&str>,
        entry_ids: &[i64],
    ) -> Result<Vec<PlaylistEntryLocation>> {
        anyhow::ensure!(playlist_id > 0, "Playlist not found");
        anyhow::ensure!(
            entry_ids.len() <= rivu_protocol::model::MAX_QUERY_ROWS,
            "Playlist entry query exceeds {} IDs",
            rivu_protocol::model::MAX_QUERY_ROWS
        );
        if entry_ids.is_empty() {
            let exists = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
                [playlist_id],
                |row| row.get::<_, bool>(0),
            )?;
            anyhow::ensure!(exists, "Playlist not found");
            return Ok(Vec::new());
        }
        let query = search_query(query);
        let total = self.playlist_match_count(playlist_id, query.as_deref())?;
        let ids = serde_json::to_string(entry_ids)?;
        // Rank only inside SQLite, before restricting the bounded identity set.
        // The unfiltered path retains indexed occurrence lookup without ranking.
        let sql = if query.is_some() {
            format!(
                "WITH matched AS (SELECT e.id,e.track_id,e.position,row_number() OVER (ORDER BY e.position,e.id)-1 AS offset FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id WHERE e.playlist_id=?1 AND {}) SELECT id,track_id,position,offset FROM matched WHERE id IN (SELECT value FROM json_each(?2)) ORDER BY position,id",
                search_filter("t", "?3")
            )
        } else {
            "SELECT id,track_id,position,position AS offset FROM playlist_entries WHERE playlist_id=?1 AND id IN (SELECT value FROM json_each(?2)) AND ?3 IS NULL ORDER BY position,id".into()
        };
        let mut statement = self.conn.prepare(&sql)?;
        Ok(statement
            .query_map(params![playlist_id, ids, query], |row| {
                Ok(PlaylistEntryLocation {
                    id: row.get(0)?,
                    playlist_id,
                    track_id: row.get(1)?,
                    position: row.get::<_, i64>(2)? as usize,
                    offset: row.get::<_, i64>(3)? as usize,
                    total,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn playlist_match_count(&self, playlist_id: i64, query: Option<&str>) -> Result<usize> {
        anyhow::ensure!(playlist_id > 0, "Playlist not found");
        let sql = if query.is_some() {
            format!(
                "SELECT (SELECT COUNT(*) FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id WHERE e.playlist_id=?1 AND {}) FROM playlists WHERE id=?1",
                search_filter("t", "?2")
            )
        } else {
            "SELECT (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1 AND ?2 IS NULL".into()
        };
        self.cached_count_for_id(
            super::CountDomain::PlaylistEntries,
            playlist_id,
            &[query.unwrap_or("")],
            || {
                Ok(self
                    .conn
                    .query_row(&sql, params![playlist_id, query], |row| {
                        row.get::<_, i64>(0)
                    })
                    .optional()?
                    .context("Playlist not found")? as usize)
            },
        )
    }

    pub fn playlist_summary(&self, playlist_id: i64) -> Result<Option<PlaylistSummary>> {
        Ok(self
            .conn
            .query_row(
                "SELECT p.id,p.name,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries e WHERE e.playlist_id=p.id)
                 FROM playlists p WHERE p.id=?1 AND p.id>0",
                [playlist_id],
                |row| {
                    Ok(PlaylistSummary {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        entry_count: row.get::<_, i64>(2)? as usize,
                    })
                },
            )
            .optional()?)
    }

    pub fn playlist_summary_page(
        &self,
        query: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<PlaylistSummary>> {
        let query = search_query(query);
        let total = self.conn.query_row(
            "SELECT COUNT(*) FROM playlists WHERE id>0 AND (?1 IS NULL OR instr(lower(name),?1)>0)",
            [query.as_deref()],
            |row| row.get::<_, i64>(0),
        )? as usize;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        let mut stmt = self.conn.prepare(
            "SELECT p.id,p.name,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries e WHERE e.playlist_id=p.id)
             FROM playlists p WHERE p.id>0 AND (?1 IS NULL OR instr(lower(p.name),?1)>0) ORDER BY p.position,p.id LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt
            .query_map(
                params![
                    query,
                    limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                |row| {
                    Ok(PlaylistSummary {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        entry_count: row.get::<_, i64>(2)? as usize,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page { total, rows })
    }

    pub fn add_playlist_sources(
        &self,
        playlist_id: i64,
        directories: &[PathBuf],
        track_ids: &[i64],
        other_roots: Option<&[PathBuf]>,
    ) -> Result<bool> {
        anyhow::ensure!(playlist_id > 0, "Playlist not found");
        if directories.is_empty() && other_roots.is_none() {
            return self.add_playlist(playlist_id, track_ids);
        }
        let tx = self.conn.unchecked_transaction()?;
        if !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
            [playlist_id],
            |row| row.get::<_, bool>(0),
        )? {
            bail!("Playlist not found");
        }
        let (directories, ids) = self.source_parameters(directories, track_ids, other_roots)?;
        // Assign positions in SQL; playlist size never determines Rust allocation.
        let inserted = tx.execute(&format!(
            "WITH selected AS (SELECT id FROM tracks WHERE {SOURCE_FILTER}), ordered AS (SELECT id,row_number() OVER (ORDER BY id)-1 AS ordinal FROM selected) INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?3,id,(SELECT COALESCE(MAX(position),-1)+1 FROM playlist_entries WHERE playlist_id=?3)+ordinal FROM ordered"
        ), params![directories, ids, playlist_id])?;
        tx.commit()?;
        Ok(inserted != 0)
    }

    pub fn playlist_summary_batch(
        &self,
        query: Option<&str>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Batch<PlaylistSummary, i64>> {
        let limit = batch_limit(limit)?;
        let query = search_query(query);
        let cursor = if after.is_some() {
            "(p.position,p.id)>(SELECT position,id FROM playlists WHERE id=?2 AND id>0)"
        } else {
            "?2 IS NULL"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT p.id,p.name,
                    (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries e WHERE e.playlist_id=p.id)
             FROM playlists p WHERE p.id>0 AND (?1 IS NULL OR instr(lower(p.name),?1)>0) AND {cursor} ORDER BY p.position,p.id LIMIT ?3",
        ))?;
        let mut db_rows = stmt.query(params![query, after, limit + 1])?;
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
        query: Option<&str>,
        after: Option<BatchContinuation<PlaylistEntryCursor>>,
        limit: usize,
        revisions: QueryStamp,
    ) -> Result<Batch<PlaylistEntryRow, BatchContinuation<PlaylistEntryCursor>>> {
        anyhow::ensure!(playlist_id > 0, "Playlist not found");
        let limit = batch_limit(limit)?;
        let query = search_query(query);
        let scope = revisions.continuation_scope(query.as_deref(), Some(playlist_id));
        if after.is_some_and(|after| after.scope != scope) {
            return Err(rivu_protocol::response::QueryError::RevisionChanged.into());
        }
        let exists = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
            [playlist_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            bail!("Playlist not found");
        }
        let filter = search_filter("t", "?2");
        let (position, id) =
            after.map_or((-1, 0), |after| (after.cursor.position, after.cursor.id));
        let start = after.map_or(0, |after| after.offset);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT e.position,e.id,e.track_id,COALESCE(t.title_override,t.raw_title),COALESCE(t.artist_override,t.raw_artist),COALESCE(t.album_override,t.raw_album),COALESCE(t.missing,1),t.favorite,t.duration,COALESCE(t.codec,''),COALESCE(t.path,''),COALESCE(t.channels,0),COALESCE(t.sample_rate,0),t.bits_per_sample,t.bitrate_bps,COALESCE(t.bitrate_exact,0),t.cue_number FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id WHERE e.playlist_id=?1 AND {filter} AND (e.position,e.id)>(?3,?4) ORDER BY e.position,e.id LIMIT ?5"
        ))?;
        let mut db_rows = stmt.query(params![playlist_id, query, position, id, limit + 1])?;
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
                position: cursor.position as usize,
                offset: if query.is_some() {
                    start
                        .checked_add(rows.len())
                        .context("Playlist matching offset overflow")?
                } else {
                    cursor.position as usize
                },
                title: row.get(3)?,
                artist: row.get(4)?,
                album: row.get(5)?,
                duration: row.get(8)?,
                codec: row.get(9)?,
                path: PathBuf::from(row.get::<_, String>(10)?),
                channels: row.get::<_, i64>(11)? as u16,
                sample_rate: row.get::<_, i64>(12)? as u32,
                bits_per_sample: row.get(13)?,
                bitrate_bps: row.get::<_, Option<i64>>(14)?.map(|value| value as u64),
                bitrate_exact: row.get(15)?,
                cue_number: row.get(16)?,
                missing: row.get::<_, i64>(6)? != 0,
                favorite: row.get(7)?,
            });
            last_cursor = Some(BatchContinuation {
                cursor,
                offset: start
                    .checked_add(rows.len())
                    .context("Playlist matching offset overflow")?,
                scope,
            });
        }
        let next = has_more.then_some(last_cursor).flatten();
        Ok(Batch { rows, next })
    }

    /// Traverse an already-validated export snapshot without rechecking its playlist.
    pub(crate) fn playlist_export_batch(
        &self,
        playlist_id: i64,
        after: Option<PlaylistEntryCursor>,
        limit: usize,
    ) -> Result<Batch<PlaylistExportTrack, PlaylistEntryCursor>> {
        anyhow::ensure!(playlist_id > 0, "Playlist not found");
        let limit = batch_limit(limit)?;
        // A LEFT JOIN keeps orphan entries visible so export fails rather than
        // silently dropping them. Project no playback, album or fingerprint data.
        let suffix = if after.is_some() {
            " AND (e.position,e.id)>(?2,?3) ORDER BY e.position,e.id LIMIT ?4"
        } else {
            " ORDER BY e.position,e.id LIMIT ?2"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT t.id,t.path,COALESCE(t.title_override,t.raw_title),
                    COALESCE(t.artist_override,t.raw_artist),t.duration,
                    t.cue_sheet,t.cue_number,t.cue_start_frame,t.cue_end_frame,
                    e.position AS entry_position,e.id AS entry_id
             FROM playlist_entries e LEFT JOIN tracks t
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
            rows.push(PlaylistExportTrack {
                path: PathBuf::from(row.get::<_, String>(1)?),
                title: row.get(2)?,
                artist: row.get(3)?,
                duration: row.get(4)?,
                cue: cue_from_row(row, 5)?,
            });
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
        query: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<PlaylistEntryRow>> {
        let query = search_query(query);
        let total = self.playlist_match_count(playlist_id, query.as_deref())?;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        let filter = search_filter("t", "?2");
        // Keep the index seek for unfiltered windows; filtered windows page matches.
        let paging = if query.is_some() {
            "ORDER BY e.position,e.id LIMIT ?4 OFFSET ?3"
        } else {
            "AND e.position>=?3 ORDER BY e.position,e.id LIMIT ?4"
        };
        let mut stmt = self.conn.prepare(&format!("SELECT e.id,e.track_id,COALESCE(t.title_override,t.raw_title),COALESCE(t.artist_override,t.raw_artist),COALESCE(t.album_override,t.raw_album),COALESCE(t.missing,1),t.favorite,e.position,t.duration,COALESCE(t.codec,''),COALESCE(t.path,''),COALESCE(t.channels,0),COALESCE(t.sample_rate,0),t.bits_per_sample,t.bitrate_bps,COALESCE(t.bitrate_exact,0),t.cue_number FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id WHERE e.playlist_id=?1 AND {filter} {paging}"))?;
        let limit = limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64;
        let offset = offset.min(i64::MAX as usize) as i64;
        let mut rows = stmt
            .query_map(params![playlist_id, query, offset, limit], |r| {
                Ok(PlaylistEntryRow {
                    id: r.get(0)?,
                    track_id: r.get(1)?,
                    position: r.get::<_, i64>(7)? as usize,
                    offset: r.get::<_, i64>(7)? as usize,
                    title: r.get(2)?,
                    artist: r.get(3)?,
                    album: r.get(4)?,
                    duration: r.get(8)?,
                    codec: r.get(9)?,
                    path: PathBuf::from(r.get::<_, String>(10)?),
                    channels: r.get::<_, i64>(11)? as u16,
                    sample_rate: r.get::<_, i64>(12)? as u32,
                    bits_per_sample: r.get(13)?,
                    bitrate_bps: r.get::<_, Option<i64>>(14)?.map(|value| value as u64),
                    bitrate_exact: r.get(15)?,
                    cue_number: r.get(16)?,
                    missing: r.get::<_, i64>(5)? != 0,
                    favorite: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if query.is_some() {
            for (index, row) in rows.iter_mut().enumerate() {
                row.offset = offset as usize + index;
            }
        }
        Ok(Page { total, rows })
    }

    #[cfg(test)]
    pub fn playlist_track_ids(&self, playlist_id: i64, limit: usize) -> Result<Vec<i64>> {
        anyhow::ensure!(playlist_id > 0, "Playlist not found");
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
        available_playlist_name(&tx, name)?;
        tx.execute("INSERT INTO playlists(name,position) SELECT ?1,COALESCE(MAX(position)+1,0) FROM playlists WHERE id>0", [name])?;
        let id = tx.last_insert_rowid();
        append_tracks(&tx, id, tracks)?;
        tx.commit()?;
        Ok(id)
    }
    /// Returns the new identity and whether the committed playlist has members.
    pub fn create_playlist_from_queue(&self, name: &str) -> Result<(i64, bool)> {
        Self::validate_playlist_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        available_playlist_name(&tx, name)?;
        tx.execute("INSERT INTO playlists(name,position) SELECT ?1,COALESCE(MAX(position)+1,0) FROM playlists WHERE id>0", [name])?;
        let id = tx.last_insert_rowid();
        let inserted = tx.execute("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?1,track_id,row_number() OVER (ORDER BY position,id)-1 FROM playlist_entries WHERE playlist_id=0 ORDER BY position,id", [id])?;
        tx.commit()?;
        Ok((id, inserted != 0))
    }
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<bool> {
        anyhow::ensure!(id > 0, "Playlist not found");
        Self::validate_playlist_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        let unchanged: bool = tx
            .query_row(
                "SELECT name=?2 FROM playlists WHERE id=?1",
                params![id, name],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("playlist {id} does not exist"))?;
        if unchanged {
            return Ok(false);
        }
        available_playlist_name(&tx, name)?;
        let updated = tx.execute(
            "UPDATE playlists SET name=?1 WHERE id=?2 AND name IS NOT ?1",
            params![name, id],
        )?;
        tx.commit()?;
        Ok(updated != 0)
    }
    /// Reorder the catalog without touching playlist occurrences or the queue.
    pub fn move_playlist(
        &self,
        playlist_id: i64,
        before_playlist_id: Option<i64>,
        after: bool,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        exists_playlist(&tx, playlist_id)?;
        if let Some(target) = before_playlist_id {
            exists_playlist(&tx, target)?;
        }
        if before_playlist_id == Some(playlist_id) {
            return Ok(false);
        }
        // Rank in SQLite so deletion gaps and stable-ID tie breaks have exactly
        // the same meaning here as in catalog queries, without loading the catalog.
        let updated = tx.execute(
            "WITH ranked AS MATERIALIZED (
                SELECT id,row_number() OVER (ORDER BY position,id)-1 AS position
                FROM playlists WHERE id>0
             ), boundary AS MATERIALIZED (
                SELECT (SELECT position FROM ranked WHERE id=?1) AS old,
                    CASE WHEN ?2 IS NULL THEN (SELECT COUNT(*) FROM ranked)
                         ELSE (SELECT position FROM ranked WHERE id=?2)+?3 END AS target
             ), bounds AS MATERIALIZED (
                SELECT old,target-(old<target) AS insertion FROM boundary
             ), moved AS (
                SELECT id,CASE
                    WHEN id=?1 THEN insertion
                    WHEN old<insertion AND position>old AND position<=insertion THEN position-1
                    WHEN old>insertion AND position>=insertion AND position<old THEN position+1
                    ELSE position END AS position
                FROM ranked CROSS JOIN bounds WHERE old<>insertion
             ) UPDATE playlists SET position=moved.position FROM moved
               WHERE playlists.id=moved.id AND playlists.position<>moved.position",
            params![playlist_id, before_playlist_id, i64::from(after)],
        )?;
        tx.commit()?;
        Ok(updated != 0)
    }

    /// Returns whether the committed deletion removed any playlist members.
    pub fn delete_playlist(&self, id: i64) -> Result<bool> {
        anyhow::ensure!(id > 0, "Playlist not found");
        let tx = self.conn.unchecked_transaction()?;
        let removed = tx.execute("DELETE FROM playlist_entries WHERE playlist_id=?", [id])?;
        changed(
            tx.execute("DELETE FROM playlists WHERE id=?", [id])?,
            "playlist",
            id,
        )?;
        tx.commit()?;
        Ok(removed != 0)
    }
    pub fn add_playlist(&self, id: i64, tracks: &[i64]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        exists_playlist(&tx, id)?;
        let inserted = append_tracks(&tx, id, tracks)?;
        tx.commit()?;
        Ok(inserted != 0)
    }

    pub(crate) fn insert_playlist_tracks(
        &self,
        playlist_id: i64,
        tracks: &[i64],
        index: usize,
    ) -> Result<bool> {
        let ids = self.validated_track_ids(tracks)?;
        self.insert_playlist(playlist_id, &ids, None, index)
    }

    pub(crate) fn insert_playlist_sources(
        &self,
        playlist_id: i64,
        directories: &[PathBuf],
        tracks: &[i64],
        other_roots: Option<&[PathBuf]>,
        index: usize,
    ) -> Result<bool> {
        if directories.is_empty() && other_roots.is_none() {
            return self.insert_playlist_tracks(playlist_id, tracks, index);
        }
        let (directories, ids) = self.source_parameters(directories, tracks, other_roots)?;
        self.insert_playlist(playlist_id, &ids, Some(&directories), index)
    }

    fn insert_playlist(
        &self,
        playlist_id: i64,
        ids: &str,
        directories: Option<&str>,
        index: usize,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        exists_playlist(&tx, playlist_id)?;
        let count = super::positions::insert_tracks(&tx, playlist_id, index, ids, directories)?;
        tx.commit()?;
        Ok(count != 0)
    }
    /// Returns the owning playlist only after the entry removal commits.
    pub fn remove_playlist_entry(&self, id: i64) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let (pid, p): (i64, i64) = tx
            .query_row(
                "SELECT playlist_id,position FROM playlist_entries WHERE id=? AND playlist_id>0",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("playlist entry {id} does not exist"))?;
        tx.execute("DELETE FROM playlist_entries WHERE id=?", [id])?;
        close_gaps(&tx, &[p], pid)?;
        tx.commit()?;
        Ok(pid)
    }
    /// Returns the owning playlist only after an actual committed reorder.
    pub fn move_playlist_entry(&self, id: i64, index: usize) -> Result<Option<i64>> {
        let tx = self.conn.unchecked_transaction()?;
        let (pid, old): (i64, i64) = tx
            .query_row(
                "SELECT playlist_id,position FROM playlist_entries WHERE id=? AND playlist_id>0",
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
        if !move_selection(&tx, pid, &[(id, old)], index as i64)? {
            return Ok(None);
        }
        tx.commit()?;
        Ok(Some(pid))
    }

    /// Keep the first occurrence of each track without replacing surviving identities.
    pub fn deduplicate_playlist(&self, playlist_id: i64) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        exists_playlist(&tx, playlist_id)?;
        let removed = tx.execute(
            "WITH ranked AS (SELECT id,row_number() OVER (PARTITION BY track_id ORDER BY position,id) AS occurrence FROM playlist_entries WHERE playlist_id=?1) DELETE FROM playlist_entries WHERE playlist_id=?1 AND id IN (SELECT id FROM ranked WHERE occurrence>1)",
            [playlist_id],
        )?;
        if removed == 0 {
            return Ok(false);
        }
        tx.execute(
            "WITH ordered AS MATERIALIZED (SELECT id,row_number() OVER (ORDER BY position,id)-1 AS position FROM playlist_entries WHERE playlist_id=?1) UPDATE playlist_entries SET position=ordered.position FROM ordered WHERE playlist_entries.id=ordered.id AND playlist_entries.playlist_id=?1 AND playlist_entries.position<>ordered.position",
            [playlist_id],
        )?;
        tx.commit()?;
        Ok(true)
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
        close_gaps(&tx, &selected, playlist_id)?;
        tx.commit()?;
        Ok(true)
    }

    /// Move stable occurrences as a group in their original playlist order.
    /// Rust retains only selected identities and positions, never the whole playlist.
    pub fn move_playlist_entries(
        &self,
        playlist_id: i64,
        entry_ids: &[i64],
        before_entry_id: Option<i64>,
        after: bool,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let ids = serde_json::to_string(entry_ids)?;
        validate_selected_entries(&tx, playlist_id, &ids)?;
        let selected = tx
            .prepare(
                "SELECT id,position FROM playlist_entries WHERE playlist_id=?1
                 AND id IN (SELECT value FROM json_each(?2)) ORDER BY position,id",
            )?
            .query_map(params![playlist_id, ids], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<(i64, i64)>>>()?;
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
        if selected.is_empty()
            || selected
                .binary_search_by_key(&target, |&(_, position)| position)
                .is_ok()
        {
            return Ok(false);
        }
        let boundary = target + i64::from(after && before_entry_id.is_some());
        let insertion =
            boundary - selected.partition_point(|&(_, position)| position < boundary) as i64;
        if !move_selection(&tx, playlist_id, &selected, insertion)? {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }
}

fn validate_selected_entries(tx: &Transaction<'_>, playlist_id: i64, ids: &str) -> Result<()> {
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
    Ok(())
}

fn selected_entries(tx: &Transaction<'_>, playlist_id: i64, ids: &str) -> Result<Vec<i64>> {
    validate_selected_entries(tx, playlist_id, ids)?;
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
    Ok(tx.execute("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?1,value,(SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1)+CAST(key AS INTEGER) FROM json_each(?2) ORDER BY CAST(key AS INTEGER)", params![id, tracks])?)
}

fn available_playlist_name(tx: &Transaction<'_>, name: &str) -> Result<()> {
    let taken: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM playlists WHERE name=?1)",
        [name],
        |row| row.get(0),
    )?;
    if taken {
        bail!("A playlist named {name:?} already exists");
    }
    Ok(())
}

fn exists_playlist(tx: &Transaction<'_>, id: i64) -> Result<()> {
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=? AND id>0)",
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
        assert_eq!(
            store
                .playlist_summary_page(None, 0, 10)?
                .rows
                .iter()
                .map(|playlist| playlist.id)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        let expected = PlaylistEntryLocation {
            id: 102,
            playlist_id: 10,
            track_id: 3,
            position: 2,
            offset: 2,
            total: 3,
        };
        assert_eq!(store.playlist_entry(102)?, Some(expected));
        assert_eq!(store.move_playlist_entry(102, 0)?, Some(10));
        assert_eq!(
            store.playlist_entry(102)?,
            Some(PlaylistEntryLocation {
                position: 0,
                offset: 0,
                ..expected
            })
        );
        assert_eq!(store.remove_playlist_entry(100)?, 10);
        assert_eq!(
            store.playlist_entry(102)?,
            Some(PlaylistEntryLocation {
                position: 0,
                offset: 0,
                total: 2,
                ..expected
            })
        );
        assert_eq!(store.playlist_entry(200)?.unwrap().total, 1);
        assert_eq!(store.remove_playlist_entry(102)?, 10);
        assert_eq!(store.playlist_entry(102)?, None);
        assert_eq!(store.playlist_entry(999)?, None);
        store.conn.execute_batch(
            "CREATE TRIGGER reject_playlist_delete BEFORE DELETE ON playlists
             WHEN OLD.id=20 BEGIN SELECT RAISE(ABORT,'blocked playlist deletion'); END;",
        )?;
        assert!(store.delete_playlist(20).is_err());
        assert_eq!(store.playlist_entry(200)?.unwrap().total, 1);
        store
            .conn
            .execute_batch("DROP TRIGGER reject_playlist_delete")?;
        assert!(store.delete_playlist(20)?);
        assert_eq!(store.playlist_entry(200)?, None);
        assert_eq!(
            store
                .playlist_summary_page(None, 0, 10)?
                .rows
                .iter()
                .map(|playlist| playlist.id)
                .collect::<Vec<_>>(),
            vec![10]
        );
        let empty = store.create_playlist("empty")?;
        assert!(!store.delete_playlist(empty)?);
        assert!(store.delete_playlist(20).is_err());
        assert_eq!(
            store
                .playlist_summary_page(None, 0, 10)?
                .rows
                .iter()
                .map(|playlist| playlist.id)
                .collect::<Vec<_>>(),
            vec![10]
        );
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
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES (300,0,2,0);
             UPDATE rivu_meta SET current_queue_entry_id=300 WHERE singleton=1;",
        )?;
        assert_eq!(store.remove_playlist_entry(101)?, 10);
        assert!(store.remove_playlist_entry(999).is_err());
        let entries = store.conn.prepare(
            "SELECT id,playlist_id,track_id,position FROM playlist_entries WHERE playlist_id>0 ORDER BY playlist_id,position",
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
            "SELECT current_queue_entry_id FROM rivu_meta WHERE singleton=1",
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
        let before = store.playlist_summary_page(None, 0, 10)?;
        let entries = store.playlist_entries_page(original, None, 0, 10)?;
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
            assert_eq!(store.playlist_summary_page(None, 0, 10)?, before);
            assert_eq!(store.playlist_entries_page(original, None, 0, 10)?, entries);
        }
        for name in ["  Mix \u{2003} ", "混音", "Mix\u{200b}"] {
            for kind in 0..3 {
                let id = match kind {
                    0 => store.create_playlist(name)?,
                    1 => store.create_playlist_with_tracks(name, &[1])?,
                    _ => store.create_playlist_from_queue(name)?.0,
                };
                let summaries = store.playlist_summary_page(None, 0, 100)?.rows;
                assert_eq!(
                    summaries.iter().find(|row| row.id == id).unwrap().name,
                    name
                );
                store.delete_playlist(id)?;
            }
            assert!(store.rename_playlist(original, name)?);
            let summaries = store.playlist_summary_page(None, 0, 100)?.rows;
            assert_eq!(
                summaries
                    .iter()
                    .find(|row| row.id == original)
                    .unwrap()
                    .name,
                name
            );
        }
        Ok(())
    }

    #[test]
    fn rename_reports_persistent_changes_and_rejects_unknown_ids() -> Result<()> {
        let store = store()?;
        let id = store.create_playlist_with_tracks("Original", &[3, 1])?;
        let entries = store.playlist_entries_page(id, None, 0, 10)?.rows;
        assert!(!store.rename_playlist(id, "Original")?);
        assert!(store.rename_playlist(id, "Renamed")?);
        assert!(!store.rename_playlist(id, "Renamed")?);
        assert!(store.rename_playlist(i64::MAX, "Renamed").is_err());
        assert_eq!(store.playlist_entries_page(id, None, 0, 10)?.rows, entries);
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
        let before = store.playlist_entries_page(id, None, 0, 10)?.rows;
        assert!(!store.add_playlist(id, &[])?);
        assert!(!store.add_playlist_sources(id, &[], &[], None)?);
        assert!(store.add_playlist(i64::MAX, &[]).is_err());
        assert!(
            store
                .add_playlist_sources(i64::MAX, &[], &[], None)
                .is_err()
        );
        assert!(store.add_playlist(id, &[1, i64::MAX]).is_err());
        assert!(
            store
                .add_playlist_sources(id, &[], &[1, i64::MAX], None)
                .is_err()
        );
        assert_eq!(store.playlist_entries_page(id, None, 0, 10)?.rows, before);
        assert!(store.add_playlist(id, &[2, 1, 2])?);
        assert!(store.add_playlist_sources(id, &[], &[1, 2], None)?);
        assert!(store.add_playlist(id, &[2, 3, 3])?);
        assert!(store.add_playlist_sources(id, &[], &[3, 4], None)?);
        assert_eq!(
            store.playlist_track_ids(id, 20)?,
            [1, 2, 2, 1, 2, 1, 2, 2, 3, 3, 3, 4]
        );
        Ok(())
    }

    #[test]
    fn deep_pages_preserve_entry_identity_missing_tracks_and_exact_totals() -> Result<()> {
        let store = store()?;
        let total = rivu_protocol::model::MAX_QUERY_ROWS + 17;
        store.conn.execute(
            "WITH RECURSIVE ids(id) AS (SELECT 5 UNION ALL SELECT id+1 FROM ids WHERE id<?1) INSERT INTO tracks(id,path) SELECT id,'track-'||id FROM ids",
            [total as i64],
        )?;
        let tracks = (1..=total as i64).collect::<Vec<_>>();
        let id = store.create_playlist_with_tracks("deep", &tracks)?;
        let tail = store.playlist_entries_page(id, None, total - 3, usize::MAX)?;
        assert_eq!(tail.total, total);
        assert_eq!(
            tail.rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
            tracks[total - 3..]
        );
        store.conn.execute(
            "UPDATE tracks SET missing=1 WHERE id=?1",
            [total as i64 - 1],
        )?;
        let page = store.playlist_entries_page(id, None, total - 3, 2)?;
        assert_eq!(page.total, total);
        assert_eq!(page.rows[0], tail.rows[0]);
        assert_eq!(page.rows[1].id, tail.rows[1].id);
        assert_eq!(page.rows[1].track_id, tail.rows[1].track_id);
        assert!(page.rows[1].missing);
        assert_eq!(page.rows[1].title, tail.rows[1].title);
        for offset in [total, total + 1, usize::MAX] {
            let page = store.playlist_entries_page(id, None, offset, 3)?;
            assert_eq!(page.total, total);
            assert!(page.rows.is_empty());
        }
        assert_eq!(
            store.playlist_entries_page(id, None, usize::MAX, 0)?.total,
            total
        );
        assert_eq!(
            store
                .playlist_entries_page(id, None, 0, usize::MAX)?
                .rows
                .len(),
            rivu_protocol::model::MAX_QUERY_ROWS
        );
        assert_eq!(
            store.playlist_summary_page(None, 0, 10)?.rows[0].entry_count,
            total
        );
        assert_eq!(
            store.playlist_summary_batch(None, None, 10)?.rows[0].entry_count,
            total
        );
        Ok(())
    }

    #[test]
    fn append_keeps_occurrence_order_and_invalid_ids_roll_back() -> Result<()> {
        let store = store()?;
        assert!(
            store
                .create_playlist_with_tracks("invalid", &[1, -1, 2])
                .is_err()
        );
        assert_eq!(store.playlist_summary_page(None, 0, 0)?.total, 0);
        let id = store.create_playlist_with_tracks("valid", &[3, 1, 3, 2, 1])?;
        let original = store.playlist_entries_page(id, None, 0, 10)?.rows;
        assert_eq!(store.playlist_track_ids(id, 10)?, vec![3, 1, 3, 2, 1]);
        assert!(store.add_playlist(id, &[4, i64::MAX, 4]).is_err());
        assert!(
            store
                .add_playlist_sources(id, &[PathBuf::new()], &[i64::MAX], None)
                .is_err()
        );
        assert_eq!(store.playlist_track_ids(id, 10)?, vec![3, 1, 3, 2, 1]);
        assert_eq!(
            store
                .playlist_entries_page(id, None, 0, 10)?
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            original.iter().map(|row| row.id).collect::<Vec<_>>()
        );
        store.add_playlist(id, &[2, 4, 3, 4])?;
        assert_eq!(
            store.playlist_track_ids(id, 10)?,
            vec![3, 1, 3, 2, 1, 2, 4, 3, 4]
        );
        Ok(())
    }

    #[test]
    fn count_only_pages_keep_totals_and_unknown_playlist_errors() -> Result<()> {
        let store = store()?;
        let id = store.create_playlist_with_tracks("entries", &[1, 2, 3])?;
        store.create_playlist("empty")?;
        let summaries = store.playlist_summary_page(None, usize::MAX, 0)?;
        assert_eq!(summaries.total, 2);
        assert!(summaries.rows.is_empty());
        let entries = store.playlist_entries_page(id, None, usize::MAX, 0)?;
        assert_eq!(entries.total, 3);
        assert!(entries.rows.is_empty());
        assert!(store.playlist_entries_page(i64::MAX, None, 0, 0).is_err());
        assert_eq!(
            store.playlist_entries_page(id, None, 1, 1)?.rows[0].track_id,
            2
        );
        assert_eq!(
            store
                .playlist_entries_page(id, None, 0, usize::MAX)?
                .rows
                .len(),
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
    use rivu_protocol::model::MAX_QUERY_ROWS;
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
        let first = store.playlist_export_batch(1, None, MAX_QUERY_ROWS)?;
        assert_eq!(first.rows.len(), MAX_QUERY_ROWS);
        assert_eq!(
            first
                .rows
                .iter()
                .map(|track| track.path.clone())
                .collect::<Vec<_>>(),
            (4..=total as i64)
                .rev()
                .map(|id| PathBuf::from(format!("track-{id}")))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            first.next,
            Some(PlaylistEntryCursor {
                position: MAX_QUERY_ROWS as i64 - 1,
                id: MAX_QUERY_ROWS as i64 + 999,
            })
        );
        let tail = store.playlist_export_batch(1, first.next, MAX_QUERY_ROWS)?;
        assert_eq!(
            tail.rows
                .iter()
                .map(|track| track.path.clone())
                .collect::<Vec<_>>(),
            [
                PathBuf::from("track-3"),
                PathBuf::from("track-2"),
                PathBuf::from("track-1")
            ]
        );
        assert!(tail.next.is_none());
        // A full final batch must terminate without an extra empty-page fetch.
        let exact_tail = store.playlist_export_batch(1, first.next, 3)?;
        assert_eq!(exact_tail.rows, tail.rows);
        assert!(exact_tail.next.is_none());
        let exhausted = store.playlist_export_batch(
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
    fn export_batches_handle_empty_lists_limits_and_orphaned_entries() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let empty = store.create_playlist("empty")?;
        let batch = store.playlist_export_batch(empty, None, 1)?;
        assert!(batch.rows.is_empty());
        assert!(batch.next.is_none());
        assert!(store.playlist_export_batch(empty, None, 0).is_err());
        assert!(
            store
                .playlist_export_batch(empty, None, MAX_QUERY_ROWS + 1)
                .is_err()
        );
        store.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES(1,'one'),(3,'three');
             INSERT INTO playlist_entries(id,playlist_id,track_id,position)
             VALUES(10,1,1,0),(20,1,2,1),(30,1,3,2);",
        )?;
        // An orphan in the lookahead must produce a cursor, not a false end of stream.
        let first = store.playlist_export_batch(empty, None, 1)?;
        assert_eq!(first.rows[0].path, Path::new("one"));
        assert!(first.next.is_some());
        for (after, limit) in [(None, 3), (first.next, 1)] {
            assert!(store.playlist_export_batch(empty, after, limit).is_err());
        }
        Ok(())
    }

    #[test]
    fn export_batches_keep_entry_id_order_when_positions_tie() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES(1,'one'),(2,'two'),(3,'three');
             INSERT INTO playlists(id,name) VALUES(1,'ties');
             INSERT INTO playlist_entries(id,playlist_id,track_id,position)
             VALUES(30,1,1,0),(10,1,3,0),(20,1,2,0);",
        )?;
        let mut after = None;
        let mut paths = Vec::new();
        loop {
            let batch = store.playlist_export_batch(1, after, 1)?;
            paths.extend(batch.rows.into_iter().map(|track| track.path));
            after = batch.next;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(
            paths,
            [
                PathBuf::from("three"),
                PathBuf::from("two"),
                PathBuf::from("one")
            ]
        );
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
    fn indexed_playlist_insertion_preserves_every_source_occurrence() -> Result<()> {
        for index in 0..=1 {
            let store = fixture()?;
            assert!(store.insert_playlist_tracks(20, &[4, 1, 2, 4, 3], index)?);
            let entries = store.playlist_entry_locations(20, None, &order(&store, 20)?)?;
            let mut expected = vec![1];
            expected.splice(index..index, [4, 1, 2, 4, 3]);
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| entry.track_id)
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                store.playlist_entry(200)?.unwrap().position,
                if index == 0 { 5 } else { 0 }
            );
            assert!(store.insert_playlist_tracks(20, &[1, 4, 4], 0)?);
            assert!(!store.insert_playlist_tracks(20, &[], 9)?);
            assert!(store.insert_playlist_tracks(20, &[1], 10).is_err());
            assert!(store.insert_playlist_tracks(20, &[], usize::MAX).is_err());
            assert!(store.insert_playlist_tracks(999, &[], 0).is_err());
            assert!(store.insert_playlist_tracks(20, &[6, 999], 0).is_err());
            assert_eq!(order(&store, 10)?, [100, 101, 102, 103, 104, 105]);
        }
        Ok(())
    }

    #[test]
    fn indexed_playlist_sources_use_union_order_and_roll_back_partial_writes() -> Result<()> {
        let store = fixture()?;
        store
            .conn
            .execute_batch("UPDATE tracks SET path='/music/'||id;")?;
        let before = order(&store, 20)?;
        store.conn.execute_batch("CREATE TRIGGER reject_playlist_insertion BEFORE INSERT ON playlist_entries WHEN NEW.playlist_id=20 AND NEW.track_id=3 BEGIN SELECT RAISE(ABORT,'reject insertion'); END;")?;
        assert!(store.insert_playlist_tracks(20, &[2, 3, 4], 0).is_err());
        assert!(
            store
                .insert_playlist_sources(20, &["/music".into()], &[6, 2, 6], None, 0)
                .is_err()
        );
        assert_eq!(order(&store, 20)?, before);
        store
            .conn
            .execute_batch("DROP TRIGGER reject_playlist_insertion;")?;
        assert!(store.insert_playlist_sources(
            20,
            &["/music".into(), "/music".into()],
            &[6, 2, 6],
            None,
            0
        )?);
        let entries = store.playlist_entry_locations(20, None, &order(&store, 20)?)?;
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 1]
        );
        assert!(!store.insert_playlist_sources(20, &[], &[], None, 7)?);
        assert!(
            store
                .insert_playlist_sources(20, &[], &[], None, 8)
                .is_err()
        );
        assert!(
            store
                .insert_playlist_sources(20, &[], &[999], None, 0)
                .is_err()
        );
        assert_eq!(order(&store, 10)?, [100, 101, 102, 103, 104, 105]);
        Ok(())
    }

    #[test]
    fn all_after_selection_shapes_match_stable_remove_then_insert_order() -> Result<()> {
        let original: Vec<i64> = (100..106).collect();
        for mask in 0..64 {
            let selected: Vec<_> = original
                .iter()
                .copied()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, id)| id)
                .collect();
            for target in original.iter().copied().map(Some).chain([None]) {
                let store = fixture()?;
                let mut expected = original.clone();
                if !target.is_some_and(|id| selected.contains(&id)) {
                    expected.retain(|id| !selected.contains(id));
                    let insertion = target.map_or(expected.len(), |id| {
                        expected.iter().position(|&entry| entry == id).unwrap() + 1
                    });
                    expected.splice(insertion..insertion, selected.iter().copied());
                }
                let submitted: Vec<_> = selected
                    .iter()
                    .rev()
                    .copied()
                    .chain(selected.iter().copied())
                    .collect();
                assert_eq!(
                    store.move_playlist_entries(10, &submitted, target, true)?,
                    expected != original
                );
                assert_eq!(
                    order(&store, 10)?,
                    expected,
                    "mask={mask}, target={target:?}"
                );
                assert_eq!(order(&store, 20)?, [200]);
            }
        }
        Ok(())
    }

    #[test]
    fn all_selection_shapes_match_stable_remove_then_insert_order() -> Result<()> {
        let original: Vec<i64> = (100..106).collect();
        for mask in 0..64 {
            let selected: Vec<_> = original
                .iter()
                .copied()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, id)| id)
                .collect();
            for target in original.iter().copied().map(Some).chain([None]) {
                let store = fixture()?;
                let mut expected = original.clone();
                if !target.is_some_and(|id| selected.contains(&id)) {
                    expected.retain(|id| !selected.contains(id));
                    let insertion = target.map_or(expected.len(), |id| {
                        expected.iter().position(|&entry| entry == id).unwrap()
                    });
                    expected.splice(insertion..insertion, selected.iter().copied());
                }
                let mut submitted: Vec<_> = selected.iter().rev().copied().collect();
                submitted.extend_from_slice(&selected);
                assert_eq!(
                    store.move_playlist_entries(10, &submitted, target, false)?,
                    expected != original
                );
                assert_eq!(
                    order(&store, 10)?,
                    expected,
                    "mask={mask}, target={target:?}"
                );
                assert_eq!(order(&store, 20)?, [200]);
                if let [id] = selected.as_slice() {
                    let position = expected.iter().position(|entry| entry == id).unwrap();
                    let single = fixture()?;
                    assert_eq!(
                        single.move_playlist_entry(*id, position)?,
                        (expected != original).then_some(10)
                    );
                    assert_eq!(order(&single, 10)?, expected);
                }
            }
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
            assert!(!store.move_playlist_entries(10, &ids, target, false)?);
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
            assert!(
                store
                    .move_playlist_entries(playlist, &ids, target, false)
                    .is_err()
            );
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
        store.conn.execute_batch(
            "INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES (300,0,2,0);",
        )?;
        assert!(store.remove_playlist_entries(10, &[103, 101, 103])?);
        assert_eq!(order(&store, 10)?, [100, 102, 104, 105]);
        assert_eq!(order(&store, 20)?, [200]);
        assert_eq!(
            store.conn.query_row(
                "SELECT track_id FROM playlist_entries WHERE playlist_id=0 AND id=300",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            2
        );
        assert!(store.track(2)?.is_some());
        Ok(())
    }

    #[test]
    fn group_mutation_failure_rolls_back_positions_and_deletes() -> Result<()> {
        let store = fixture()?;
        let before = order(&store, 10)?;
        store.conn.execute_batch(
            "CREATE TRIGGER reject_move BEFORE UPDATE OF position ON playlist_entries
             WHEN OLD.id=103 BEGIN SELECT RAISE(ABORT,'reject move'); END;",
        )?;
        assert!(
            store
                .move_playlist_entries(10, &[101, 103], None, false)
                .is_err()
        );
        assert_eq!(order(&store, 10)?, before);
        store.conn.execute_batch(
            "DROP TRIGGER reject_move;
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
                .move_playlist_entries(10, &[100, 100], Some(103), false)
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
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) SELECT 1000+n,30,1,n-1 FROM entries;",
        )?;
        let ids: Vec<_> = (1001..=1000 + rivu_protocol::model::MAX_QUERY_ROWS as i64).collect();
        let locations = store.playlist_entry_locations(30, None, &ids)?;
        assert_eq!(locations.len(), rivu_protocol::model::MAX_QUERY_ROWS);
        assert!(
            locations
                .iter()
                .enumerate()
                .all(|(index, row)| row.position == index && row.total == 1000)
        );
        assert!(
            store
                .playlist_entry_locations(
                    30,
                    None,
                    &vec![1001; rivu_protocol::model::MAX_QUERY_ROWS + 1]
                )
                .is_err()
        );
        assert!(store.playlist_entry_locations(999, None, &[]).is_err());
        assert!(store.playlist_entry_locations(30, None, &[])?.is_empty());
        assert_eq!(
            store
                .playlist_entry_locations(10, None, &[103, 200, 999, 103])?
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [103]
        );
        store.move_playlist_entries(10, &[103], Some(100), false)?;
        let row = store.playlist_entry_locations(10, None, &[103])?.remove(0);
        assert_eq!(
            (row.id, row.track_id, row.position, row.total),
            (103, 4, 0, 6)
        );
        Ok(())
    }
}

#[cfg(test)]
mod occurrence_tests {
    use super::*;
    use rivu_protocol::model::{CollectionDestination, CollectionSource};
    use std::path::Path;

    fn fixture() -> Result<Store> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES(1,'one'),(2,'two'),(3,'three'),(4,'four');",
        )?;
        Ok(store)
    }

    #[test]
    fn dedup_keeps_first_occurrences_and_isolates_queue_and_other_playlists() -> Result<()> {
        let store = fixture()?;
        let playlist = store.create_playlist_with_tracks("target", &[1, 2, 1, 3, 2, 1, 4])?;
        let other = store.create_playlist_with_tracks("other", &[2, 2, 1])?;
        store.enqueue_tracks(&[1, 2, 1])?;
        let current = store.queue_ids()?[2];
        store.set_queue_current(Some(current))?;
        let late_occurrence = store.playlist_entries_page(playlist, None, 5, 1)?.rows[0].id;
        store.move_playlist_entry(late_occurrence, 0)?;
        let before = store.playlist_entries_page(playlist, None, 0, 20)?.rows;
        let other_before = store.playlist_entries_page(other, None, 0, 20)?;
        let queue = store.queue_page(None, 0, 20)?;
        let state = store.queue_state()?;
        assert!(store.deduplicate_playlist(playlist)?);
        let rows = store.playlist_entries_page(playlist, None, 0, 20)?.rows;
        assert_eq!(
            rows.iter()
                .map(|row| (row.id, row.track_id, row.position))
                .collect::<Vec<_>>(),
            [0, 2, 4, 6]
                .iter()
                .enumerate()
                .map(|(position, &index)| (before[index].id, before[index].track_id, position))
                .collect::<Vec<_>>()
        );
        let changes = store.conn.total_changes();
        assert!(!store.deduplicate_playlist(playlist)?);
        let empty = store.create_playlist("empty")?;
        assert!(!store.deduplicate_playlist(empty)?);
        assert_eq!(store.conn.total_changes(), changes + 1);
        assert_eq!(
            store.playlist_entries_page(other, None, 0, 20)?,
            other_before
        );
        assert_eq!(store.queue_page(None, 0, 20)?, queue);
        assert_eq!(store.queue_state()?, state);
        assert!(store.deduplicate_queue()?);
        assert_eq!(store.queue_state()?.current_id, Some(current));
        assert_eq!(store.queue_ids()?[0], current);
        assert_eq!(
            store.playlist_entries_page(playlist, None, 0, 20)?.rows,
            rows
        );
        Ok(())
    }

    #[test]
    fn dedup_rolls_back_deletion_and_compaction_failures() -> Result<()> {
        for trigger in [
            "CREATE TEMP TRIGGER reject_dedup BEFORE DELETE ON playlist_entries WHEN old.playlist_id>0 AND old.position=4 BEGIN SELECT RAISE(ABORT,'reject delete'); END;",
            "CREATE TEMP TRIGGER reject_dedup BEFORE UPDATE OF position ON playlist_entries WHEN new.playlist_id>0 BEGIN SELECT RAISE(ABORT,'reject compact'); END;",
        ] {
            let store = fixture()?;
            let playlist = store.create_playlist_with_tracks("target", &[1, 2, 1, 3, 2, 1, 4])?;
            let before = store.playlist_entries_page(playlist, None, 0, 20)?;
            store.conn.execute_batch(trigger)?;
            assert!(store.deduplicate_playlist(playlist).is_err());
            assert_eq!(store.playlist_entries_page(playlist, None, 0, 20)?, before);
            store.conn.execute_batch("DROP TRIGGER reject_dedup;")?;
            assert!(store.deduplicate_playlist(playlist)?);
        }
        Ok(())
    }

    #[test]
    fn queue_is_hidden_and_rejected_by_ordinary_playlist_apis() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[1, 1, 2])?;
        let queue = store.queue_page(None, 0, 20)?;
        let state = store.queue_state()?;
        assert!(store.playlist_ids()?.is_empty());
        assert_eq!(store.playlist_summary_page(None, 0, 20)?.total, 0);
        assert!(
            store
                .playlist_summary_batch(None, None, 20)?
                .rows
                .is_empty()
        );
        assert!(store.playlist_entry(queue.rows[0].id as i64)?.is_none());
        assert!(store.playlist_entry_locations(0, None, &[]).is_err());
        assert!(store.playlist_entries_page(0, None, 0, 0).is_err());
        assert!(
            store
                .playlist_entry_batch(0, None, None, 1, QueryStamp::default())
                .is_err()
        );
        assert!(store.playlist_export_batch(0, None, 1).is_err());
        assert!(store.rename_playlist(0, "hidden").is_err());
        assert!(store.delete_playlist(0).is_err());
        assert!(store.add_playlist(0, &[]).is_err());
        assert!(store.add_playlist_sources(0, &[], &[1], None).is_err());
        assert!(store.insert_playlist_tracks(0, &[1], 0).is_err());
        assert!(
            store
                .insert_playlist_sources(0, &[], &[1], None, 0)
                .is_err()
        );
        assert!(
            store
                .remove_playlist_entry(queue.rows[0].id as i64)
                .is_err()
        );
        assert!(
            store
                .move_playlist_entry(queue.rows[0].id as i64, 1)
                .is_err()
        );
        assert!(store.remove_playlist_entries(0, &[]).is_err());
        assert!(store.move_playlist_entries(0, &[], None, false).is_err());
        assert!(store.deduplicate_playlist(0).is_err());
        assert!(store.deduplicate_playlist(999).is_err());
        assert!(store.replace_queue_from_playlist(0).is_err());
        assert_eq!(store.queue_page(None, 0, 20)?, queue);
        assert_eq!(store.queue_state()?, state);
        Ok(())
    }

    #[test]
    fn collection_copies_preserve_duplicates_and_never_alias_identities() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[3, 1, 3, 2, 1])?;
        let queue_ids = store.queue_ids()?;
        let (source, _) = store.create_playlist_from_queue("copy")?;
        let source_entries = store.playlist_entries_page(source, None, 0, 20)?.rows;
        assert_eq!(store.playlist_track_ids(source, 20)?, [3, 1, 3, 2, 1]);
        assert!(
            source_entries
                .iter()
                .all(|entry| !queue_ids.contains(&(entry.id as u64)))
        );
        let destination = store.create_playlist_with_tracks("destination", &[4, 4])?;
        let selection = CollectionSource::Playlist {
            playlist_id: source,
            revision: 0,
        };
        assert_eq!(
            store.transfer_collection(
                &selection,
                CollectionDestination::Playlist {
                    playlist_id: destination,
                    revision: 0,
                    index: Some(1)
                }
            )?,
            5
        );
        assert_eq!(
            store.playlist_track_ids(destination, 20)?,
            [4, 3, 1, 3, 2, 1, 4]
        );
        assert_eq!(
            store.transfer_collection(
                &selection,
                CollectionDestination::Queue { insertion: None }
            )?,
            5
        );
        assert_eq!(
            store
                .queue_page(None, 0, 20)?
                .rows
                .iter()
                .map(|row| row.track_id)
                .collect::<Vec<_>>(),
            [3, 1, 3, 2, 1, 3, 1, 3, 2, 1]
        );
        store.replace_queue_from_playlist(source)?;
        let copied_queue = store.queue_page(None, 0, 20)?;
        store.remove_playlist_entry(source_entries[0].id)?;
        assert_eq!(store.queue_page(None, 0, 20)?, copied_queue);
        let foreign = source_entries[1].id as u64;
        assert!(store.set_queue_current(Some(foreign)).is_err());
        assert!(store.remove_queue_entries(&[foreign]).is_err());
        assert!(store.move_queue_entries(&[foreign], 0).is_err());
        assert!(store.queue_track_id(foreign).is_err());
        assert!(store.queue_entries(None, &[foreign])?.is_empty());
        let queue_entry = copied_queue.rows[0].id as i64;
        assert!(
            store
                .remove_playlist_entries(source, &[queue_entry])
                .is_err()
        );
        assert!(
            store
                .move_playlist_entries(source, &[queue_entry], None, false)
                .is_err()
        );
        assert_eq!(store.queue_page(None, 0, 20)?, copied_queue);
        Ok(())
    }

    #[test]
    fn membership_impacts_separate_queue_from_ordinary_playlists() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[1, 1])?;
        let first = store.create_playlist_with_tracks("first", &[1, 1, 2])?;
        let other = store.create_playlist_with_tracks("other", &[1, 3, 1])?;
        let edit = store.edit_track(1, Some("changed"), None, None, None)?;
        assert!(edit.queue);
        let mut playlists = edit.playlists.unwrap();
        playlists.sort_unstable();
        assert_eq!(playlists, [first, other]);
        let favorite = store.set_favorite(&[1], true)?;
        assert!(favorite.queue);
        let mut playlists = favorite.playlists.unwrap();
        playlists.sort_unstable();
        assert_eq!(playlists, [first, other]);
        let removal = store.prepare_track_removal(&[1])?;
        let impact = store.remove_prepared_tracks(removal)?;
        assert!(impact.queue);
        assert_eq!(impact.playlists, [first, other]);
        assert_eq!(store.playlist_track_ids(first, 20)?, [2]);
        assert_eq!(store.playlist_track_ids(other, 20)?, [3]);
        assert_eq!(store.queue_state()?.total, 0);
        Ok(())
    }
}
