use super::rows::{
    LIBRARY_ROW_COLUMNS, LIBRARY_ROW_COLUMNS_QUALIFIED, TRACK_COLUMNS, library_row_at,
    library_row_from_row, track_from_row,
};
use super::{Store, batch_limit, changed};
use crate::model::{Batch, HistoryEntry, Page, Track};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const TRACK_SEARCH_FILTER: &str = "WHERE (?1 IS NULL OR instr(lower(COALESCE(tracks.title_override,tracks.raw_title) || ' ' || COALESCE(tracks.artist_override,tracks.raw_artist) || ' ' || tracks.path),?1)>0) AND (?2 IS NULL OR tracks.favorite=?2) AND (?3 IS NULL OR tracks.missing=?3)";
pub(super) const SOURCE_FILTER: &str = "tracks.id IN (SELECT value FROM json_each(?2)) OR EXISTS (SELECT 1 FROM json_each(?1) AS directory WHERE substr(tracks.path,1,length(directory.value))=directory.value)";

fn directory_prefix(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(String::new());
    }
    let path = crate::library::logical_path(path)?;
    let path = path
        .to_str()
        .context("Library directory path is not UTF-8")?;
    Ok(format!("{}/", path.trim_end_matches('/')))
}

fn library_order(sort: crate::model::LibrarySort) -> &'static str {
    match sort {
        crate::model::LibrarySort::Id => "id",
        crate::model::LibrarySort::Album => {
            "COALESCE(album_override,raw_album), CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(disc_number,4294967295) END, CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(track_number,4294967295) END, COALESCE(title_override,raw_title),id"
        }
        crate::model::LibrarySort::MostPlayed => "play_count DESC,id",
    }
}

impl Store {
    pub fn set_favorite(&self, ids: &[i64], favorite: bool) -> Result<()> {
        for id in ids {
            self.track_cache.borrow_mut().entries.remove(id);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut exists = tx.prepare("SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)")?;
        let mut update =
            tx.prepare("UPDATE tracks SET favorite=?1 WHERE id=?2 AND favorite<>?1")?;
        for &id in ids {
            if !exists.query_row([id], |row| row.get::<_, bool>(0))? {
                bail!("Track {id} does not exist");
            }
            update.execute(params![favorite, id])?;
        }
        drop(update);
        drop(exists);
        tx.commit()?;
        Ok(())
    }

    /// Loads only display metadata for queued IDs; unlike `track`, this never touches the full-track LRU.
    pub fn queue_rows(&self, ids: &[i64]) -> Result<Vec<crate::model::LibraryRow>> {
        let ids = serde_json::to_string(ids)?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {LIBRARY_ROW_COLUMNS} FROM tracks WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY id"
        ))?;
        Ok(stmt
            .query_map([ids], library_row_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn library_view_page(
        &self,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
        offset: usize,
        limit: usize,
    ) -> Result<Page<crate::model::LibraryRow>> {
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let total = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks {TRACK_SEARCH_FILTER}"),
            params![query, favorite, missing],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let order = library_order(sort);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {LIBRARY_ROW_COLUMNS} FROM tracks {TRACK_SEARCH_FILTER} ORDER BY {order} LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = stmt
            .query_map(
                params![
                    query,
                    favorite,
                    missing,
                    limit.min(crate::model::MAX_QUERY_ROWS) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                library_row_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page { total, rows })
    }

    pub fn library_root_page(
        &self,
        path: &Path,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
        offset: usize,
        limit: usize,
    ) -> Result<Page<crate::model::LibraryRow>> {
        let prefix = directory_prefix(path)?;
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let scope = " AND substr(tracks.path,1,length(?4))=?4";
        let total = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks {TRACK_SEARCH_FILTER}{scope}"),
            params![query, favorite, missing, prefix],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let order = library_order(sort);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {LIBRARY_ROW_COLUMNS} FROM tracks {TRACK_SEARCH_FILTER}{scope} ORDER BY {order} LIMIT ?5 OFFSET ?6"
        ))?;
        let rows = stmt
            .query_map(
                params![
                    query,
                    favorite,
                    missing,
                    prefix,
                    limit.min(crate::model::MAX_QUERY_ROWS) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                library_row_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page { total, rows })
    }

    /// One aggregate query for all roots; overlapping and duplicate inputs remain independent.
    pub fn library_root_counts(
        &self,
        roots: &[PathBuf],
    ) -> Result<Vec<crate::model::LibraryRootCount>> {
        if roots.len() > crate::model::MAX_QUERY_ROWS {
            bail!(
                "Library root count request exceeds {} roots",
                crate::model::MAX_QUERY_ROWS
            );
        }
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let prefixes = roots
            .iter()
            .map(|path| directory_prefix(path))
            .collect::<Result<Vec<_>>>()?;
        let prefixes = serde_json::to_string(&prefixes)?;
        let mut stmt = self.conn.prepare(
            "SELECT roots.key,COUNT(tracks.id) FROM json_each(?1) AS roots LEFT JOIN tracks ON substr(tracks.path,1,length(roots.value))=roots.value GROUP BY roots.key ORDER BY roots.key"
        )?;
        Ok(stmt
            .query_map([prefixes], |row| {
                let index = row.get::<_, i64>(0)? as usize;
                Ok(crate::model::LibraryRootCount {
                    path: roots[index].clone(),
                    total: row.get::<_, i64>(1)? as usize,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn track_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |row| {
                row.get::<_, i64>(0)
            })? as usize)
    }

    pub fn library_stats(&self) -> Result<crate::model::LibraryStats> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*),COALESCE(SUM(play_count),0) FROM tracks",
            [],
            |row| {
                Ok(crate::model::LibraryStats {
                    total: row.get::<_, i64>(0)? as usize,
                    play_count: row.get::<_, i64>(1)? as u64,
                })
            },
        )?)
    }

    pub fn directory_page(
        &self,
        path: &Path,
        query: Option<&str>,
        favorite: Option<bool>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<crate::model::DirectoryRow>> {
        use crate::model::DirectoryRow;
        let prefix = directory_prefix(path)?;
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        // SQLite derives immediate children; Rust retains only the requested page.
        // Group directories but preserve distinct CUE tracks sharing a source path.
        let sources =
            "WITH sources AS (SELECT id,path,substr(path,length(?4)+1) AS rest FROM tracks ";
        let children = " AND substr(path,1,length(?4))=?4), children AS (SELECT 0 AS kind,CASE WHEN ?4='' AND instr(rest,'/')=1 THEN '/' ELSE ?4||substr(rest,1,instr(rest,'/')-1) END AS child,MIN(id) AS track_id FROM sources WHERE instr(rest,'/')>0 GROUP BY child UNION ALL SELECT 1,path,id FROM sources WHERE instr(rest,'/')=0)";
        let page_columns = " SELECT c.kind,c.child,";
        let page_tail = " FROM children c JOIN tracks t ON t.id=c.track_id ORDER BY c.kind,c.child,t.id LIMIT ?5 OFFSET ?6";
        // Reuse the library search predicate and one SQL buffer for count and payload.
        let mut sql = String::with_capacity(
            sources.len()
                + TRACK_SEARCH_FILTER.len()
                + children.len()
                + page_columns.len()
                + LIBRARY_ROW_COLUMNS_QUALIFIED.len()
                + page_tail.len(),
        );
        sql.push_str(sources);
        sql.push_str(TRACK_SEARCH_FILTER);
        sql.push_str(children);
        let children_end = sql.len();
        sql.push_str(" SELECT COUNT(*) FROM children");
        let total = self.conn.query_row(
            &sql,
            params![query, favorite, None::<bool>, prefix],
            |row| row.get::<_, i64>(0),
        )? as usize;
        sql.truncate(children_end);
        sql.push_str(page_columns);
        sql.push_str(LIBRARY_ROW_COLUMNS_QUALIFIED);
        sql.push_str(page_tail);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    query,
                    favorite,
                    None::<bool>,
                    prefix,
                    limit.min(crate::model::MAX_QUERY_ROWS) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                |row| {
                    if row.get::<_, i64>(0)? == 0 {
                        Ok(DirectoryRow::Directory {
                            path: PathBuf::from(row.get::<_, String>(1)?),
                        })
                    } else {
                        Ok(DirectoryRow::Track(library_row_at(row, 2)?))
                    }
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page { total, rows })
    }

    pub(super) fn source_parameters(
        &self,
        directories: &[PathBuf],
        track_ids: &[i64],
    ) -> Result<(String, String)> {
        let mut exists = self
            .conn
            .prepare("SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)")?;
        for &id in track_ids {
            if !exists.query_row([id], |row| row.get::<_, bool>(0))? {
                bail!("Track {id} does not exist");
            }
        }
        let prefixes = directories
            .iter()
            .map(|path| directory_prefix(path))
            .collect::<Result<Vec<_>>>()?;
        Ok((
            serde_json::to_string(&prefixes)?,
            serde_json::to_string(track_ids)?,
        ))
    }

    pub fn source_track_ids(
        &self,
        directories: &[PathBuf],
        track_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>> {
        let (directories, ids) = self.source_parameters(directories, track_ids)?;
        let count = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks WHERE {SOURCE_FILTER}"),
            params![directories, ids],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let limit = limit.min(4096);
        if count > limit {
            bail!(
                "Selection contains {count} tracks, exceeding the remaining queue capacity of {limit}"
            );
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id FROM tracks WHERE {SOURCE_FILTER} ORDER BY id LIMIT ?3"
        ))?;
        Ok(stmt
            .query_map(params![directories, ids, limit as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn track_id_for_source(&self, path: &Path, cue_number: Option<u32>) -> Result<Option<i64>> {
        let path = path.to_string_lossy();
        let id = match cue_number {
            None => self
                .conn
                .query_row(
                    "SELECT id FROM tracks WHERE path=?1 AND cue_sheet IS NULL AND missing=0",
                    [path.as_ref()],
                    |row| row.get(0),
                )
                .optional()?,
            Some(number) => self
                .conn
                .query_row(
                    "SELECT id FROM tracks WHERE cue_sheet=?1 AND cue_number=?2 AND missing=0",
                    params![path.as_ref(), number],
                    |row| row.get(0),
                )
                .optional()?,
        };
        Ok(id)
    }

    pub fn library_page(
        &self,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<Track>> {
        let pattern = query
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);
        let sql = format!(
            "SELECT {TRACK_COLUMNS} FROM tracks {TRACK_SEARCH_FILTER} ORDER BY id LIMIT ?4 OFFSET ?5"
        );
        let limit = limit.min(crate::model::MAX_QUERY_ROWS) as i64;
        let offset = offset.min(i64::MAX as usize) as i64;
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![pattern, favorite, missing, limit, offset],
                track_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks {TRACK_SEARCH_FILTER}"),
            params![pattern, favorite, missing],
            |r| r.get(0),
        )?;
        Ok(Page {
            total: total as usize,
            rows,
        })
    }

    pub fn track_batch(
        &self,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Batch<Track, i64>> {
        let limit = batch_limit(limit)?;
        let pattern = query
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);
        let mut stmt = if after.is_some() {
            self.conn.prepare(&format!(
                "SELECT {TRACK_COLUMNS} FROM tracks {TRACK_SEARCH_FILTER} AND tracks.id>?4 ORDER BY tracks.id LIMIT ?5"
            ))?
        } else {
            self.conn.prepare(&format!(
                "SELECT {TRACK_COLUMNS} FROM tracks {TRACK_SEARCH_FILTER} ORDER BY tracks.id LIMIT ?4"
            ))?
        };
        let mut db_rows = if let Some(after) = after {
            stmt.query(params![pattern, favorite, missing, after, limit + 1])?
        } else {
            stmt.query(params![pattern, favorite, missing, limit + 1])?
        };
        let mut rows = Vec::with_capacity(limit as usize);
        let mut has_more = false;
        while let Some(row) = db_rows.next()? {
            if rows.len() == limit as usize {
                has_more = true;
                break;
            }
            rows.push(track_from_row(row)?);
        }
        let next = has_more
            .then(|| rows.last().map(|track| track.id))
            .flatten();
        Ok(Batch { rows, next })
    }

    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let mut q = self.conn.prepare(
            "SELECT id,COALESCE(title_override,raw_title),play_count,last_played FROM tracks
             WHERE last_played IS NOT NULL ORDER BY last_played DESC,id DESC LIMIT ?",
        )?;
        Ok(q.query_map([limit.min(i64::MAX as usize) as i64], |r| {
            Ok(HistoryEntry {
                track_id: r.get(0)?,
                title: r.get(1)?,
                play_count: r.get::<_, i64>(2)? as u64,
                played_at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn edit_track(&self, id: i64, title: &str, artist: &str, album: &str) -> Result<()> {
        self.track_cache.borrow_mut().entries.remove(&id);
        changed(
            self.conn.execute(
                "UPDATE tracks SET title_override=?,artist_override=?,album_override=? WHERE id=?",
                params![title, artist, album, id],
            )?,
            "track",
            id,
        )
    }
    pub fn remove_tracks(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.track_cache.borrow_mut().entries.remove(id);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut affected = HashSet::new();
        let mut exists = tx.prepare("SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)")?;
        let mut playlists =
            tx.prepare("SELECT DISTINCT playlist_id FROM playlist_entries WHERE track_id=?1")?;
        let mut delete_entries = tx.prepare("DELETE FROM playlist_entries WHERE track_id=?1")?;
        let mut delete_tracks = tx.prepare("DELETE FROM tracks WHERE id=?1")?;
        for &id in ids {
            if !exists.query_row([id], |row| row.get::<_, bool>(0))? {
                bail!("Track {id} does not exist");
            }
            affected.extend(
                playlists
                    .query_map([id], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
            delete_entries.execute([id])?;
            delete_tracks.execute([id])?;
        }
        drop(delete_tracks);
        drop(delete_entries);
        drop(playlists);
        drop(exists);
        for playlist in affected {
            tx.execute("WITH ordered AS (SELECT id, ROW_NUMBER() OVER (ORDER BY position,id)-1 AS new_position FROM playlist_entries WHERE playlist_id=?1) UPDATE playlist_entries SET position=(SELECT new_position FROM ordered WHERE ordered.id=playlist_entries.id) WHERE playlist_id=?1", [playlist])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_missing_tracks(&self) -> Result<()> {
        self.track_cache.borrow_mut().entries.clear();
        let tx = self.conn.unchecked_transaction()?;
        // SQLite owns the affected playlist set; Rust never collects missing IDs.
        tx.execute_batch(
            "CREATE TEMP TABLE removed_playlists(playlist_id INTEGER PRIMARY KEY);
             INSERT INTO removed_playlists SELECT DISTINCT playlist_id FROM playlist_entries WHERE track_id IN (SELECT id FROM tracks WHERE missing=1);
             DELETE FROM playlist_entries WHERE track_id IN (SELECT id FROM tracks WHERE missing=1);
             DELETE FROM tracks WHERE missing=1;
             WITH ordered AS (SELECT id,ROW_NUMBER() OVER (PARTITION BY playlist_id ORDER BY position,id)-1 AS new_position FROM playlist_entries WHERE playlist_id IN (SELECT playlist_id FROM removed_playlists))
             UPDATE playlist_entries SET position=(SELECT new_position FROM ordered WHERE ordered.id=playlist_entries.id) WHERE playlist_id IN (SELECT playlist_id FROM removed_playlists);
             DROP TABLE removed_playlists;"
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn mark_played(&self, track: i64, at: i64) -> Result<()> {
        self.track_cache.borrow_mut().entries.remove(&track);
        changed(
            self.conn.execute(
                "UPDATE tracks SET last_played=? WHERE id=?",
                params![at, track],
            )?,
            "track",
            track,
        )
    }
    pub fn increment_play_count(&self, track: i64) -> Result<()> {
        self.track_cache.borrow_mut().entries.remove(&track);
        changed(
            self.conn.execute(
                "UPDATE tracks SET play_count=play_count+1 WHERE id=?",
                [track],
            )?,
            "track",
            track,
        )
    }
}
