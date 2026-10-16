use super::positions::{PositionScope, close_gaps};
use super::rows::{
    LIBRARY_ROW_COLUMNS, LIBRARY_ROW_COLUMNS_QUALIFIED, TRACK_COLUMNS, library_row_at,
    library_row_from_row, track_from_row,
};
use super::{Store, batch_limit};
use crate::model::{Batch, DecoderHint, HistoryEntry, Page, Track};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const TRACK_SEARCH_FILTER: &str = "WHERE (?1 IS NULL OR instr(lower(COALESCE(tracks.title_override,tracks.raw_title) || ' ' || COALESCE(tracks.artist_override,tracks.raw_artist) || ' ' || tracks.path),?1)>0) AND (?2 IS NULL OR tracks.favorite=?2) AND (?3 IS NULL OR tracks.missing=?3)";
pub(super) const SOURCE_FILTER: &str = "tracks.id IN (SELECT value FROM json_each(?2) UNION SELECT scoped.id FROM json_each(?1) AS directory CROSS JOIN tracks AS scoped INDEXED BY tracks_directory WHERE directory.value<>'' AND scoped.path>=directory.value AND scoped.path<(rtrim(directory.value,'/')||'0') UNION SELECT id FROM tracks WHERE EXISTS(SELECT 1 FROM json_each(?1) WHERE value=''))";
// Normalized nonempty prefixes end in ASCII '/'; replacing that final byte with
// '0' gives the exclusive BINARY upper bound, including every Unicode suffix.
const DIRECTORY_SCOPE: &str = " AND tracks.path>=?4 AND tracks.path<(rtrim(?4,'/')||'0')";
const DIRECTORY_CHILDREN: &str = "(SELECT 0 AS kind,CASE WHEN ?4='' AND instr(rest,'/')=1 THEN '/' ELSE ?4||substr(rest,1,instr(rest,'/')-1) END AS child,MIN(id) AS track_id FROM sources WHERE instr(rest,'/')>0 GROUP BY child UNION ALL SELECT 1,path,id FROM sources WHERE instr(rest,'/')=0)";

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

/// Consumed immediately by the same core dispatch; stopping playback cannot
/// change the validated catalog identities. The JSON and cache IDs stay paired.
pub(crate) struct PreparedTrackRemoval<'a> {
    ids: &'a [i64],
    encoded: String,
}

/// Only committed deletion effects leave the transaction; no projection reads follow it.
pub struct TrackRemovalImpact {
    pub removed: usize,
    pub playlists: Vec<i64>,
    pub duplicate_groups: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackEditImpact {
    pub changed: bool,
    pub library: bool,
    pub member_facts: bool,
    /// None means the changed playlist scopes exceeded the publication bound.
    pub playlists: Option<Vec<i64>>,
}

#[derive(Debug, Default)]
pub struct FavoriteImpact {
    pub changed: bool,
    pub tracks: Option<Vec<i64>>,
    pub playlists: Option<Vec<i64>>,
}

/// Captured before loading audio; bitrate persistence must still match this source.
pub(crate) struct PlaybackSource {
    pub(crate) id: i64,
    pub(crate) path: PathBuf,
    pub(crate) file_size: u64,
    pub(crate) modified_ns: i64,
    cue: Option<crate::model::CueSegment>,
    fingerprint: Option<blake3::Hash>,
}

// Only the latest scope is retained; no row payloads or per-query map accumulate.
pub(super) struct TrackCount {
    changes: u64,
    data_version: i64,
    query: Option<String>,
    favorite: Option<bool>,
    missing: Option<bool>,
    prefix: Option<String>,
    total: usize,
}

impl Store {
    pub(crate) fn playback_source(&self, track: &Track) -> Result<Option<PlaybackSource>> {
        if track.bitrate_bps.is_some() && track.bitrate_exact {
            return Ok(None);
        }
        let fingerprint = track
            .fingerprint
            .as_deref()
            .map(blake3::Hash::from_hex)
            .transpose()
            .context("Invalid playback BLAKE3 fingerprint")?;
        let cue = track.cue.as_ref();
        let metadata = self
            .conn
            .query_row(
                "SELECT file_size,modified_ns FROM tracks WHERE id=?1 AND path=?2
                 AND fingerprint IS ?3 AND cue_sheet IS ?4 AND cue_number IS ?5
                 AND cue_start_frame IS ?6 AND cue_end_frame IS ?7
                 AND missing=0 AND (bitrate_bps IS NULL OR bitrate_exact=0)",
                params![
                    track.id,
                    track
                        .path
                        .to_str()
                        .context("Playback source path is not UTF-8")?,
                    fingerprint.as_ref().map(|hash| hash.as_bytes().as_slice()),
                    cue.and_then(|cue| cue.sheet.to_str()),
                    cue.map(|cue| cue.number),
                    cue.map(|cue| cue.start_frame as i64),
                    cue.and_then(|cue| cue.end_frame).map(|frame| frame as i64),
                ],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(metadata.map(|(file_size, modified_ns)| PlaybackSource {
            id: track.id,
            path: track.path.clone(),
            file_size,
            modified_ns,
            cue: track.cue.clone(),
            fingerprint,
        }))
    }

    /// Source-conditional enrichment: exact replaces estimates; estimates only fill gaps.
    pub(crate) fn upgrade_bitrate(
        &self,
        source: &PlaybackSource,
        bitrate_bps: u64,
        bitrate_exact: bool,
    ) -> Result<bool> {
        if bitrate_bps == 0 {
            return Ok(false);
        }
        let bitrate = i64::try_from(bitrate_bps).context("Audio bitrate exceeds database range")?;
        let cue = source.cue.as_ref();
        let updated = self.conn.execute(
            "UPDATE tracks SET bitrate_bps=?1,bitrate_exact=?11 WHERE id=?2
             AND (bitrate_bps IS NULL OR (bitrate_exact=0 AND ?11=1))
             AND (bitrate_bps IS NOT ?1 OR bitrate_exact IS NOT ?11)
             AND missing=0 AND path=?3 AND file_size=?4 AND modified_ns=?5
             AND fingerprint IS ?6 AND cue_sheet IS ?7 AND cue_number IS ?8
             AND cue_start_frame IS ?9 AND cue_end_frame IS ?10",
            params![
                bitrate,
                source.id,
                source
                    .path
                    .to_str()
                    .context("Playback source path is not UTF-8")?,
                source.file_size as i64,
                source.modified_ns,
                source
                    .fingerprint
                    .as_ref()
                    .map(|hash| hash.as_bytes().as_slice()),
                cue.and_then(|cue| cue.sheet.to_str()),
                cue.map(|cue| cue.number),
                cue.map(|cue| cue.start_frame as i64),
                cue.and_then(|cue| cue.end_frame).map(|frame| frame as i64),
                bitrate_exact,
            ],
        )?;
        if updated != 0 {
            self.track_cache.borrow_mut().entries.remove(&source.id);
        }
        Ok(updated != 0)
    }

    pub fn set_favorite(&self, ids: &[i64], favorite: bool) -> Result<FavoriteImpact> {
        let tx = self.conn.unchecked_transaction()?;
        let encoded_ids = self.validated_track_ids(ids)?;
        let mut impact = favorite_metadata(&tx, &encoded_ids, Some(favorite))?;
        let updated = tx.execute(
            "UPDATE tracks SET favorite=?1 WHERE favorite<>?1 AND id IN (SELECT value FROM json_each(?2))",
            params![favorite, encoded_ids],
        )?;
        tx.commit()?;
        if updated != 0 {
            let mut cache = self.track_cache.borrow_mut();
            for id in ids {
                if cache
                    .entries
                    .get(id)
                    .is_some_and(|(track, _)| track.favorite != favorite)
                {
                    cache.entries.remove(id);
                }
            }
        }
        impact.changed = updated != 0;
        Ok(impact)
    }

    pub fn toggle_favorite(&self, ids: &[i64]) -> Result<FavoriteImpact> {
        let tx = self.conn.unchecked_transaction()?;
        let encoded_ids = self.validated_track_ids(ids)?;
        let mut impact = favorite_metadata(&tx, &encoded_ids, None)?;
        let updated = tx.execute(
            "UPDATE tracks SET favorite=NOT favorite WHERE id IN (SELECT value FROM json_each(?1))",
            params![encoded_ids],
        )?;
        tx.commit()?;
        if updated != 0 {
            let mut cache = self.track_cache.borrow_mut();
            for id in ids {
                cache.entries.remove(id);
            }
        }
        impact.changed = updated != 0;
        Ok(impact)
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
        let total = self.filtered_track_count(query.as_deref(), favorite, missing, None)?;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
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

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep the view-page filter and paging contract with one additional root path"
    )]
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
        if prefix.is_empty() {
            return self.library_view_page(query, favorite, missing, sort, offset, limit);
        }
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let scope = DIRECTORY_SCOPE;
        let total =
            self.filtered_track_count(query.as_deref(), favorite, missing, Some(&prefix))?;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
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

    #[expect(
        clippy::too_many_arguments,
        reason = "Match the library projection filters when resolving one exact identity"
    )]
    pub fn library_location(
        &self,
        track_id: i64,
        path: &Path,
        tree: bool,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: crate::model::LibrarySort,
    ) -> Result<crate::model::LibraryLocateResult> {
        use crate::model::{LibraryLocateResult, LibraryLocation};
        let Some(source) = self
            .conn
            .query_row("SELECT path FROM tracks WHERE id=?1", [track_id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
        else {
            return Ok(LibraryLocateResult::Unavailable);
        };
        let prefix = directory_prefix(path)?;
        let Some(rest) = source.strip_prefix(&prefix).filter(|rest| !rest.is_empty()) else {
            return Ok(LibraryLocateResult::OutsideRoot);
        };
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let matches = self.conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM tracks {TRACK_SEARCH_FILTER} AND id=?4)"),
            params![query, favorite, missing, track_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !matches {
            return Ok(LibraryLocateResult::Filtered);
        }
        let child = if tree {
            rest.find('/')
                .map(|separator| PathBuf::from(format!("{}{}", prefix, &rest[..separator])))
        } else {
            None
        };
        let (sql, target) = if tree {
            (
                format!(
                    "WITH sources AS (SELECT id,path,substr(path,length(?4)+1) AS rest FROM tracks {TRACK_SEARCH_FILTER}{DIRECTORY_SCOPE}), children AS {DIRECTORY_CHILDREN}, ranked AS (SELECT kind,child,track_id,row_number() OVER (ORDER BY kind,child,track_id)-1 AS position,COUNT(*) OVER () AS total FROM children) SELECT position,total FROM ranked WHERE (?5 IS NOT NULL AND kind=0 AND child=?5) OR (?5 IS NULL AND kind=1 AND track_id=?6)"
                ),
                child.as_deref().and_then(Path::to_str),
            )
        } else {
            (
                format!(
                    "WITH ranked AS (SELECT id,row_number() OVER (ORDER BY {})-1 AS position,COUNT(*) OVER () AS total FROM tracks {TRACK_SEARCH_FILTER}{DIRECTORY_SCOPE}) SELECT position,total FROM ranked WHERE id=?6 AND ?5 IS NULL",
                    library_order(sort)
                ),
                None,
            )
        };
        let location = self
            .conn
            .query_row(
                &sql,
                params![query, favorite, missing, prefix, target, track_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as usize,
                        row.get::<_, i64>(1)? as usize,
                    ))
                },
            )
            .optional()?;
        Ok(
            location.map_or(LibraryLocateResult::Unavailable, |(offset, total)| {
                LibraryLocateResult::Located(LibraryLocation {
                    offset,
                    total,
                    child,
                })
            }),
        )
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
        let mut unique = Vec::with_capacity(prefixes.len());
        let mut indices = HashMap::with_capacity(prefixes.len());
        for prefix in &prefixes {
            if let std::collections::hash_map::Entry::Vacant(entry) = indices.entry(prefix.as_str())
            {
                entry.insert(unique.len());
                unique.push(prefix.as_str());
            }
        }
        let mut stmt = self.conn.prepare(
            "SELECT roots.key,CASE WHEN roots.value='' THEN (SELECT COUNT(*) FROM tracks) ELSE (SELECT COUNT(*) FROM tracks WHERE tracks.path>=roots.value AND tracks.path<(rtrim(roots.value,'/')||'0')) END FROM json_each(?1) AS roots ORDER BY roots.key"
        )?;
        let totals = stmt
            .query_map([serde_json::to_string(&unique)?], |row| {
                Ok(row.get::<_, i64>(1)? as usize)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(roots
            .iter()
            .zip(prefixes.iter())
            .map(|(path, prefix)| crate::model::LibraryRootCount {
                path: path.clone(),
                total: totals[indices[prefix.as_str()]],
            })
            .collect())
    }

    pub(crate) fn track_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |row| {
                row.get::<_, i64>(0)
            })? as usize)
    }

    fn filtered_track_count(
        &self,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        prefix: Option<&str>,
    ) -> Result<usize> {
        if query.is_none() && favorite.is_none() && missing.is_none() && prefix.is_none() {
            return self.track_count();
        }
        // total_changes detects local writes, data_version other connections' commits.
        // Never retain or reuse an explicit transaction's snapshot: a rollback does
        // not advance either counter after that snapshot was read.
        let cacheable = self.conn.is_autocommit();
        let changes = self.conn.total_changes();
        let data_version = self
            .conn
            .prepare_cached("PRAGMA data_version")?
            .query_row([], |row| row.get::<_, i64>(0))?;
        if cacheable
            && let Some(cached) = self.count_cache.borrow().as_ref()
            && cached.changes == changes
            && cached.data_version == data_version
            && cached.query.as_deref() == query
            && cached.favorite == favorite
            && cached.missing == missing
            && cached.prefix.as_deref() == prefix
        {
            return Ok(cached.total);
        }
        let total = match prefix {
            Some(prefix) if query.is_none() && favorite.is_none() && missing.is_none() => {
                // Omitting nullable predicates keeps this count on the covering path index.
                self.conn.query_row(
                    "SELECT COUNT(*) FROM tracks WHERE path>=?1 AND path<(rtrim(?1,'/')||'0')",
                    [prefix],
                    |row| row.get::<_, i64>(0),
                )?
            }
            Some(prefix) => self.conn.query_row(
                &format!("SELECT COUNT(*) FROM tracks {TRACK_SEARCH_FILTER}{DIRECTORY_SCOPE}"),
                params![query, favorite, missing, prefix],
                |row| row.get::<_, i64>(0),
            )?,
            None => self.conn.query_row(
                &format!("SELECT COUNT(*) FROM tracks {TRACK_SEARCH_FILTER}"),
                params![query, favorite, missing],
                |row| row.get::<_, i64>(0),
            )?,
        } as usize;
        if cacheable {
            *self.count_cache.borrow_mut() = Some(TrackCount {
                changes,
                data_version,
                query: query.map(str::to_owned),
                favorite,
                missing,
                prefix: prefix.map(str::to_owned),
                total,
            });
        }
        Ok(total)
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
        missing: Option<bool>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<crate::model::DirectoryRow>> {
        use crate::model::DirectoryRow;
        let prefix = directory_prefix(path)?;
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let scope = if prefix.is_empty() {
            ""
        } else {
            DIRECTORY_SCOPE
        };
        // Materialize narrow children once for both the exact total and the page.
        // Group directories but preserve distinct CUE tracks sharing a source path.
        let sources =
            "WITH sources AS (SELECT id,path,substr(path,length(?4)+1) AS rest FROM tracks ";
        let children_prefix = "), children AS ";
        let children = DIRECTORY_CHILDREN;
        let page_columns = ", page AS MATERIALIZED (SELECT kind,child,track_id FROM children ORDER BY kind,child,track_id LIMIT ?5 OFFSET ?6) SELECT total.total,p.kind,p.child,";
        // Join payload only after pagination; empty pages retain the total row.
        let page_tail = " FROM (SELECT COUNT(*) AS total FROM children) total LEFT JOIN page p ON 1 LEFT JOIN tracks t ON p.kind=1 AND t.id=p.track_id ORDER BY p.kind,p.child,p.track_id";
        let count_tail = " SELECT COUNT(*) FROM children";
        let materialized = if limit == 0 { "" } else { "MATERIALIZED " };
        let mut sql = String::with_capacity(
            sources.len()
                + TRACK_SEARCH_FILTER.len()
                + scope.len()
                + children_prefix.len()
                + materialized.len()
                + children.len()
                + if limit == 0 {
                    count_tail.len()
                } else {
                    page_columns.len() + LIBRARY_ROW_COLUMNS_QUALIFIED.len() + page_tail.len()
                },
        );
        sql.push_str(sources);
        sql.push_str(TRACK_SEARCH_FILTER);
        sql.push_str(scope);
        sql.push_str(children_prefix);
        sql.push_str(materialized);
        sql.push_str(children);
        if limit == 0 {
            // A single consumer needs no materialized children or page machinery.
            sql.push_str(count_tail);
            let total =
                self.conn
                    .query_row(&sql, params![query, favorite, missing, prefix], |row| {
                        row.get::<_, i64>(0)
                    })? as usize;
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        sql.push_str(page_columns);
        sql.push_str(LIBRARY_ROW_COLUMNS_QUALIFIED);
        sql.push_str(page_tail);
        let mut stmt = self.conn.prepare(&sql)?;
        let mut result = stmt.query(params![
            query,
            favorite,
            missing,
            prefix,
            limit.min(crate::model::MAX_QUERY_ROWS) as i64,
            offset.min(i64::MAX as usize) as i64
        ])?;
        let mut page = Page {
            total: 0,
            rows: Vec::new(),
        };
        while let Some(row) = result.next()? {
            page.total = row.get::<_, i64>(0)? as usize;
            let Some(kind) = row.get::<_, Option<i64>>(1)? else {
                continue;
            };
            page.rows.push(if kind == 0 {
                DirectoryRow::Directory {
                    path: PathBuf::from(row.get::<_, String>(2)?),
                }
            } else {
                DirectoryRow::Track(library_row_at(row, 3)?)
            });
        }
        Ok(page)
    }

    pub(super) fn source_parameters(
        &self,
        directories: &[PathBuf],
        track_ids: &[i64],
    ) -> Result<(String, String)> {
        let ids = self.validated_track_ids(track_ids)?;
        let prefixes = directories
            .iter()
            .map(|path| directory_prefix(path))
            .collect::<Result<Vec<_>>>()?;
        Ok((serde_json::to_string(&prefixes)?, ids))
    }
    pub(crate) fn prepare_track_removal<'a>(
        &self,
        ids: &'a [i64],
    ) -> Result<PreparedTrackRemoval<'a>> {
        Ok(PreparedTrackRemoval {
            ids,
            encoded: self.validated_track_ids(ids)?,
        })
    }

    pub(super) fn validated_track_ids(&self, ids: &[i64]) -> Result<String> {
        let encoded = serde_json::to_string(ids)?;
        validate_encoded_track_ids(&self.conn, &encoded)?;
        Ok(encoded)
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
        let total = self.filtered_track_count(pattern.as_deref(), favorite, missing, None)?;
        if limit == 0 {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
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
        Ok(Page { total, rows })
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
            "SELECT id,COALESCE(title_override,raw_title),play_count,last_played,favorite FROM tracks
             WHERE last_played IS NOT NULL ORDER BY last_played DESC,id DESC LIMIT ?",
        )?;
        Ok(q.query_map([limit.min(i64::MAX as usize) as i64], |r| {
            Ok(HistoryEntry {
                track_id: r.get(0)?,
                title: r.get(1)?,
                play_count: r.get::<_, i64>(2)? as u64,
                played_at: r.get(3)?,
                favorite: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn edit_track(
        &self,
        id: i64,
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        decoder_hint: Option<DecoderHint>,
    ) -> Result<TrackEditImpact> {
        let tx = self.conn.unchecked_transaction()?;
        let decoder_hint = decoder_hint.map(|hint| match hint {
            DecoderHint::Auto => 0,
            DecoderHint::Native => 1,
            DecoderHint::Ffmpeg => 2,
        });
        let (mut impact, playlist_values_changed) = tx.query_row(
            "SELECT
                ((?1 IS NOT NULL AND ?1 IS NOT COALESCE(title_override,raw_title))
                 OR (?2 IS NOT NULL AND ?2 IS NOT COALESCE(artist_override,raw_artist))
                 OR (?3 IS NOT NULL AND ?3 IS NOT COALESCE(album_override,raw_album))),
                (?4 IS NOT NULL AND ?4 IS NOT decoder_hint),
                ((title_override IS NOT NULL OR artist_override IS NOT NULL
                  OR album_override IS NOT NULL OR decoder_hint<>0)
                 IS NOT
                 (COALESCE(?1,title_override) IS NOT NULL OR COALESCE(?2,artist_override) IS NOT NULL
                  OR COALESCE(?3,album_override) IS NOT NULL OR COALESCE(?4,decoder_hint)<>0))
             FROM tracks WHERE id=?5",
            params![title, artist, album, decoder_hint, id],
            |row| {
                let display: bool = row.get(0)?;
                let decoder: bool = row.get(1)?;
                Ok((TrackEditImpact { library: display || decoder, member_facts: row.get(2)?, ..Default::default() }, display))
            },
        ).optional()?.with_context(|| format!("track {id} does not exist"))?;
        if impact.library {
            impact.playlists = if playlist_values_changed {
                bounded_metadata_ids(
                    &tx,
                    "SELECT DISTINCT playlist_id FROM playlist_entries WHERE track_id=?1",
                    [id],
                )?
            } else {
                Some(Vec::new())
            };
        }
        let updated = tx.execute(
            "UPDATE tracks SET title_override=COALESCE(?1,title_override),
             artist_override=COALESCE(?2,artist_override),
             album_override=COALESCE(?3,album_override),
             decoder_hint=COALESCE(?4,decoder_hint) WHERE id=?5 AND
             ((?1 IS NOT NULL AND title_override IS NOT ?1) OR
              (?2 IS NOT NULL AND artist_override IS NOT ?2) OR
              (?3 IS NOT NULL AND album_override IS NOT ?3) OR
              (?4 IS NOT NULL AND decoder_hint IS NOT ?4))",
            params![title, artist, album, decoder_hint, id],
        )?;
        tx.commit()?;
        impact.changed = updated != 0;
        if impact.library {
            self.track_cache.borrow_mut().entries.remove(&id);
        }
        Ok(impact)
    }
    pub fn remove_tracks(&self, ids: &[i64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let removal = self.prepare_track_removal(ids)?;
        self.remove_track_rows(&tx, removal)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn remove_prepared_tracks(
        &self,
        removal: PreparedTrackRemoval<'_>,
    ) -> Result<TrackRemovalImpact> {
        let tx = self.conn.unchecked_transaction()?;
        let removed = removal.ids.len();
        let duplicate_groups = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE id IN (SELECT value FROM json_each(?1)) AND cue_sheet IS NULL AND fingerprint IS NOT NULL)",
            [&removal.encoded], |row| row.get(0),
        )?;
        let playlists = self.remove_track_rows(&tx, removal)?;
        tx.commit()?;
        Ok(TrackRemovalImpact {
            removed,
            playlists,
            duplicate_groups,
        })
    }

    fn remove_track_rows(
        &self,
        tx: &Transaction<'_>,
        removal: PreparedTrackRemoval<'_>,
    ) -> Result<Vec<i64>> {
        for id in removal.ids {
            self.track_cache.borrow_mut().entries.remove(id);
        }
        let ids = removal.encoded;
        let queue_positions = tx
            .prepare("SELECT position FROM queue_entries WHERE track_id IN (SELECT value FROM json_each(?1)) ORDER BY position")?
            .query_map([&ids], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        let playlist_positions = removed_playlist_positions(
            tx,
            "SELECT playlist_id,position FROM playlist_entries WHERE track_id IN (SELECT value FROM json_each(?1)) ORDER BY playlist_id,position",
            [&ids],
        )?;
        tx.execute(
            "DELETE FROM playlist_entries WHERE track_id IN (SELECT value FROM json_each(?1))",
            [&ids],
        )?;
        tx.execute(
            "DELETE FROM tracks WHERE id IN (SELECT value FROM json_each(?1))",
            [&ids],
        )?;
        close_gaps(tx, &queue_positions, PositionScope::Queue)?;
        let mut playlists = Vec::with_capacity(playlist_positions.len());
        for (playlist_id, positions) in playlist_positions {
            close_gaps(tx, &positions, PositionScope::Playlist(playlist_id))?;
            playlists.push(playlist_id);
        }
        super::queue::invalidate_structure(tx, !queue_positions.is_empty())?;
        Ok(playlists)
    }

    pub fn remove_missing_tracks(&self) -> Result<TrackRemovalImpact> {
        self.track_cache.borrow_mut().entries.clear();
        let tx = self.conn.unchecked_transaction()?;
        let queue_positions = tx
            .prepare("SELECT position FROM queue_entries WHERE track_id IN (SELECT id FROM tracks WHERE missing=1) ORDER BY position")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        let playlist_positions = removed_playlist_positions(
            &tx,
            "SELECT playlist_id,position FROM playlist_entries WHERE track_id IN (SELECT id FROM tracks WHERE missing=1) ORDER BY playlist_id,position",
            [],
        )?;
        let duplicate_groups = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE missing=1 AND cue_sheet IS NULL AND fingerprint IS NOT NULL)",
            [], |row| row.get(0),
        )?;
        tx.execute(
            "DELETE FROM playlist_entries WHERE track_id IN (SELECT id FROM tracks WHERE missing=1)",
            [],
        )?;
        let removed = tx.execute("DELETE FROM tracks WHERE missing=1", [])?;
        close_gaps(&tx, &queue_positions, PositionScope::Queue)?;
        let mut playlists = Vec::with_capacity(playlist_positions.len());
        for (playlist_id, positions) in playlist_positions {
            close_gaps(&tx, &positions, PositionScope::Playlist(playlist_id))?;
            playlists.push(playlist_id);
        }
        super::queue::invalidate_structure(&tx, !queue_positions.is_empty())?;
        tx.commit()?;
        Ok(TrackRemovalImpact {
            removed,
            playlists,
            duplicate_groups,
        })
    }
    pub fn mark_played(&self, track: i64, at: i64) -> Result<super::TrackStats> {
        let stats = self
            .conn
            .query_row(
                "UPDATE tracks SET last_played=?1 WHERE id=?2 RETURNING play_count,last_played",
                params![at, track],
                super::TrackStats::from_row,
            )
            .optional()?
            .with_context(|| format!("track {track} does not exist"))?;
        self.cache_track_stats(track, stats);
        Ok(stats)
    }
    pub fn increment_play_count(&self, track: i64) -> Result<super::TrackStats> {
        let stats = self.conn.query_row(
            "UPDATE tracks SET play_count=play_count+1 WHERE id=?1 RETURNING play_count,last_played",
            [track], super::TrackStats::from_row,
        ).optional()?.with_context(|| format!("track {track} does not exist"))?;
        self.cache_track_stats(track, stats);
        Ok(stats)
    }

    fn cache_track_stats(&self, id: i64, stats: super::TrackStats) {
        if let Some((track, _)) = self.track_cache.borrow_mut().entries.get_mut(&id) {
            track.play_count = stats.play_count;
            track.last_played = stats.last_played;
        }
    }
}

fn bounded_metadata_ids(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Option<Vec<i64>>> {
    let mut statement = conn.prepare(sql)?;
    let ids = statement
        .query_map(params, |row| row.get(0))?
        .take(crate::model::MAX_METADATA_SCOPES + 1)
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok((ids.len() <= crate::model::MAX_METADATA_SCOPES).then_some(ids))
}

fn favorite_metadata(
    conn: &Connection,
    encoded_ids: &str,
    favorite: Option<bool>,
) -> Result<FavoriteImpact> {
    let tracks = bounded_metadata_ids(
        conn,
        "SELECT id FROM tracks WHERE id IN (SELECT value FROM json_each(?1)) AND (?2 IS NULL OR favorite<>?2)",
        params![encoded_ids, favorite],
    )?;
    let playlists = match &tracks {
        Some(ids) if ids.is_empty() => Some(Vec::new()),
        Some(_) => bounded_metadata_ids(
            conn,
            "SELECT DISTINCT p.playlist_id FROM tracks AS t JOIN playlist_entries AS p ON p.track_id=t.id WHERE t.id IN (SELECT value FROM json_each(?1)) AND (?2 IS NULL OR t.favorite<>?2)",
            params![encoded_ids, favorite],
        )?,
        None => None,
    };
    Ok(FavoriteImpact {
        tracks,
        playlists,
        ..Default::default()
    })
}

pub(super) fn validate_encoded_track_ids(conn: &Connection, ids: &str) -> Result<()> {
    let missing: Option<i64> = conn
        .query_row(
            "SELECT value FROM json_each(?1) WHERE value NOT IN (SELECT id FROM tracks) LIMIT 1",
            [ids],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = missing {
        bail!("Track {id} does not exist");
    }
    Ok(())
}

fn removed_playlist_positions(
    tx: &Transaction<'_>,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(i64, Vec<i64>)>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map(params, |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut removed: Vec<(i64, Vec<i64>)> = Vec::new();
    for row in rows {
        let (playlist_id, position) = row?;
        if let Some((id, positions)) = removed.last_mut()
            && *id == playlist_id
        {
            positions.push(position);
        } else {
            removed.push((playlist_id, vec![position]));
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod deletion_tests {
    use super::*;

    fn fixture() -> Result<Store> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES
                (1,'one'),(2,'two'),(3,'three'),(4,'four'),
                (5,'five'),(6,'six'),(7,'seven'),(8,'unreferenced');
             INSERT INTO queue_entries(id,track_id,position) VALUES
                (100,1,0),(101,2,1),(102,2,2),(103,3,3),
                (104,4,4),(105,5,5),(106,6,6),(107,7,7);
             UPDATE queue_meta SET current_id=106 WHERE singleton=1;
             INSERT INTO playlists(id,name) VALUES (10,'first'),(20,'second'),(30,'untouched');
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES
                (200,10,1,0),(201,10,2,1),(202,10,3,2),(203,10,4,3),
                (204,10,5,4),(205,10,6,5),(206,10,7,6),
                (300,20,5,0),(301,20,7,1),(302,20,2,2),(303,20,6,3),
                (304,20,3,4),(305,20,1,5),(306,20,4,6),
                (400,30,4,0),(401,30,6,1),(402,30,7,2);",
        )?;
        Ok(store)
    }

    fn queue_rows(store: &Store) -> Result<Vec<(i64, i64, i64)>> {
        Ok(store
            .conn
            .prepare("SELECT id,track_id,position FROM queue_entries ORDER BY position")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn playlist_rows(store: &Store) -> Result<Vec<(i64, i64, i64, i64)>> {
        Ok(store.conn.prepare("SELECT id,playlist_id,track_id,position FROM playlist_entries ORDER BY playlist_id,position")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn current(store: &Store) -> Result<Option<i64>> {
        Ok(store.conn.query_row(
            "SELECT current_id FROM queue_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?)
    }

    fn assert_compacted(store: &Store) -> Result<()> {
        assert_eq!(
            queue_rows(store)?,
            vec![(100, 1, 0), (104, 4, 1), (106, 6, 2), (107, 7, 3)]
        );
        assert_eq!(
            playlist_rows(store)?,
            vec![
                (200, 10, 1, 0),
                (203, 10, 4, 1),
                (205, 10, 6, 2),
                (206, 10, 7, 3),
                (301, 20, 7, 0),
                (303, 20, 6, 1),
                (305, 20, 1, 2),
                (306, 20, 4, 3),
                (400, 30, 4, 0),
                (401, 30, 6, 1),
                (402, 30, 7, 2),
            ]
        );
        Ok(())
    }

    #[test]
    fn prepared_deletion_compacts_gaps_after_playback_statistics_change() -> Result<()> {
        let store = fixture()?;
        let ids = [5, 2, 3, 2, 5];
        let removal = store.prepare_track_removal(&ids)?;
        store.mark_played(2, 123)?;
        store.increment_play_count(2)?;
        store.remove_prepared_tracks(removal)?;
        assert_compacted(&store)?;
        assert_eq!(current(&store)?, Some(106));
        Ok(())
    }

    #[test]
    fn missing_cleanup_compacts_all_references_and_clears_removed_current() -> Result<()> {
        let store = fixture()?;
        store.conn.execute_batch(
            "UPDATE tracks SET missing=1 WHERE id IN (2,3,5);
             UPDATE queue_meta SET current_id=102 WHERE singleton=1;",
        )?;
        store.remove_missing_tracks()?;
        assert_compacted(&store)?;
        assert_eq!(current(&store)?, None);
        let remaining_missing: i64 =
            store
                .conn
                .query_row("SELECT COUNT(*) FROM tracks WHERE missing=1", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(remaining_missing, 0);
        store.remove_missing_tracks()?;
        assert_compacted(&store)?;
        Ok(())
    }

    #[test]
    fn unknown_ids_roll_back_and_unreferenced_deletions_preserve_order() -> Result<()> {
        let store = fixture()?;
        let queue = queue_rows(&store)?;
        let playlists = playlist_rows(&store)?;
        let revision: i64 = store.conn.query_row(
            "SELECT revision FROM queue_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        assert!(store.remove_tracks(&[2, 999, 2]).is_err());
        store.remove_tracks(&[])?;
        store.remove_tracks(&[8, 8])?;
        assert_eq!(queue_rows(&store)?, queue);
        assert_eq!(playlist_rows(&store)?, playlists);
        assert_eq!(current(&store)?, Some(106));
        let after_revision: i64 = store.conn.query_row(
            "SELECT revision FROM queue_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(after_revision, revision);
        let track_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))?;
        assert_eq!(track_count, 7);
        Ok(())
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn no_op_edits_preserve_existing_tracks_but_reject_unknown_ids() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute(
            "INSERT INTO tracks(id,path,raw_title,raw_artist,raw_album) VALUES(1,'one','Title','Artist','Album')",
            [],
        )?;
        let original = store.track(1)?.unwrap();
        assert!(!store.edit_track(1, None, None, None, None)?.changed);
        assert_eq!(store.track(1)?.unwrap(), original);
        assert!(store.edit_track(999, None, None, None, None).is_err());
        assert!(
            store
                .edit_track(999, Some("Title"), None, None, None)
                .is_err()
        );
        assert!(
            store
                .edit_track(1, Some(""), Some("Override"), None, None)?
                .changed
        );
        let edited = store.track(1)?.unwrap();
        assert_eq!(
            (&*edited.title, &*edited.artist, &*edited.album),
            ("", "Override", "Album")
        );
        assert!(
            !store
                .edit_track(1, Some(""), Some("Override"), None, None)?
                .changed
        );
        assert_eq!(store.track(1)?.unwrap(), edited);
        assert!(
            store
                .edit_track(1, None, None, Some("New album"), None)?
                .changed
        );
        let edited = store.track(1)?.unwrap();
        assert_eq!(
            (&*edited.title, &*edited.artist, &*edited.album),
            ("", "Override", "New album")
        );
        Ok(())
    }

    #[test]
    fn effective_noop_override_changes_member_facts_without_display_invalidation() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute(
            "INSERT INTO tracks(id,path,raw_title,raw_album) VALUES(1,'one','Title','Album')",
            [],
        )?;
        store.enqueue_tracks(&[1])?;
        let track = store.track(1)?.unwrap();
        let queue = store.queue_state()?;
        assert_eq!(
            store.edit_track(1, Some("Title"), None, None, None)?,
            TrackEditImpact {
                changed: true,
                member_facts: true,
                library: false,
                ..Default::default()
            }
        );
        assert_eq!(store.track(1)?.unwrap(), track);
        assert_eq!(store.queue_state()?, queue);
        assert_eq!(
            store.edit_track(1, Some("Title"), None, None, None)?,
            TrackEditImpact::default()
        );
        assert_eq!(
            store.edit_track(1, None, None, Some("Album"), None)?,
            TrackEditImpact {
                changed: true,
                ..Default::default()
            }
        );
        assert_eq!(store.queue_state()?, queue);
        Ok(())
    }

    #[test]
    fn invalid_track_requests_have_no_effect_and_duplicate_ids_are_accepted() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store
            .conn
            .execute_batch("INSERT INTO tracks(id,path) VALUES (1,'one'),(2,'two');")?;
        for ids in [&[1, -1, 1][..], &[2, i64::MAX][..]] {
            assert!(store.set_favorite(ids, true).is_err());
            assert!(store.remove_tracks(ids).is_err());
            assert!(store.source_parameters(&[], ids).is_err());
        }
        assert_eq!(store.track_count()?, 2);
        let favorites: i64 =
            store
                .conn
                .query_row("SELECT COUNT(*) FROM tracks WHERE favorite=1", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(favorites, 0);
        assert!(store.set_favorite(&[2, 1, 2], true)?.changed);
        assert!(!store.set_favorite(&[2, 1, 2], true)?.changed);
        assert!(!store.set_favorite(&[], false)?.changed);
        assert_eq!(store.library_stats()?.total, 2);
        Ok(())
    }

    #[test]
    fn directory_pages_distinguish_filesystem_root_from_empty_prefix() -> Result<()> {
        use crate::model::DirectoryRow;
        let store = Store::open(Path::new(":memory:"))?;
        let empty = store.directory_page(Path::new(""), None, None, None, 0, 10)?;
        assert_eq!(empty.total, 0);
        assert!(empty.rows.is_empty());
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path) VALUES
             (1,'/music/one.wav'),(2,'/z.wav'),(3,'relative/one.wav'),
             (4,'standalone.wav'),(5,'relative0/outside.wav');",
        )?;
        let all = store.directory_page(Path::new(""), None, None, None, 0, 10)?;
        assert_eq!(all.total, 4);
        assert_eq!(
            &all.rows[..3],
            &[
                DirectoryRow::Directory { path: "/".into() },
                DirectoryRow::Directory {
                    path: "relative".into(),
                },
                DirectoryRow::Directory {
                    path: "relative0".into(),
                },
            ]
        );
        assert!(matches!(&all.rows[3], DirectoryRow::Track(row) if row.id == 4));
        let root = store.directory_page(Path::new("/"), None, None, None, 0, 10)?;
        assert_eq!(root.total, 2);
        assert_eq!(
            root.rows[0],
            DirectoryRow::Directory {
                path: "/music".into(),
            }
        );
        assert!(matches!(&root.rows[1], DirectoryRow::Track(row) if row.id == 2));
        for (path, total) in [("", 4), ("/", 2)] {
            for (offset, limit) in [(0, 0), (total, 10)] {
                let page =
                    store.directory_page(Path::new(path), None, None, None, offset, limit)?;
                assert_eq!(page.total, total);
                assert!(page.rows.is_empty());
            }
        }
        Ok(())
    }

    #[test]
    fn indexed_directory_ranges_preserve_literal_unicode_and_case_boundaries() -> Result<()> {
        use crate::model::{DirectoryRow, LibrarySort};
        let store = Store::open(Path::new(":memory:"))?;
        for root in ["/m%_usic", "/音楽🎵", "/a\\b", "/Upper"] {
            let included: i64 = store.conn.query_row(
                "INSERT INTO tracks(path) VALUES(?1) RETURNING id",
                [format!("{root}/one.wav")],
                |row| row.get(0),
            )?;
            for path in [
                format!("{root}0/outside.wav"),
                format!("{root}al/outside.wav"),
            ] {
                store
                    .conn
                    .execute("INSERT INTO tracks(path) VALUES(?1)", [path])?;
            }
            let path = Path::new(root);
            assert_eq!(
                store.library_root_counts(&[path.to_path_buf()])?[0].total,
                1
            );
            let page = store.library_root_page(path, None, None, None, LibrarySort::Id, 0, 10)?;
            assert_eq!(page.total, 1);
            assert_eq!(
                page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
                [included]
            );
            let tree = store.directory_page(path, None, None, None, 0, 10)?;
            assert_eq!(tree.total, 1);
            assert!(matches!(&tree.rows[..], [DirectoryRow::Track(row)] if row.id == included));
            let queue_before = store.queue_state()?.total;
            store.enqueue_sources(
                &[path.to_path_buf(), path.to_path_buf()],
                &[included, included],
            )?;
            let queued = store.queue_page(queue_before, 10)?;
            assert_eq!(
                queued
                    .rows
                    .iter()
                    .map(|row| row.track_id)
                    .collect::<Vec<_>>(),
                [included]
            );
            let playlist = store.create_playlist("Literal range")?;
            store.add_playlist_sources(playlist, &[path.to_path_buf()], &[included])?;
            assert_eq!(store.playlist_track_ids(playlist, 10)?, [included]);
        }
        store.conn.execute(
            "INSERT INTO tracks(path) VALUES('/upper/not-capitalized.wav')",
            [],
        )?;
        assert_eq!(
            store.library_root_counts(&[PathBuf::from("/Upper")])?[0].total,
            1
        );
        let playlist = store.create_playlist("All")?;
        store.add_playlist_sources(playlist, &[PathBuf::new(), "/Upper".into()], &[1, 1])?;
        assert_eq!(
            store.playlist_track_ids(playlist, 100)?,
            (1..=13).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn root_aliases_preserve_input_paths_overlap_and_all_track_kinds() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let roots = [
            "/music",
            "/music/",
            "/music/disc/..",
            "/music/./disc",
            "/music/disc",
            "/musical",
            "/empty",
            "",
            "/",
        ]
        .map(PathBuf::from);
        let empty = store.library_root_counts(&roots)?;
        assert!(empty.iter().all(|count| count.total == 0));
        assert_eq!(
            empty.iter().map(|count| &count.path).collect::<Vec<_>>(),
            roots.iter().collect::<Vec<_>>()
        );
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path,missing,cue_sheet,cue_number) VALUES
                (1,'/music/one.wav',0,NULL,NULL),
                (2,'/music/disc/image.wav',1,'/music/disc/image.cue',1),
                (3,'/music/disc/image.wav',0,'/music/disc/image.cue',2),
                (4,'/musical/other.wav',0,NULL,NULL);",
        )?;
        let counts = store.library_root_counts(&roots)?;
        assert_eq!(
            counts.iter().map(|count| count.total).collect::<Vec<_>>(),
            vec![3, 3, 3, 2, 2, 1, 0, 4, 4]
        );
        assert_eq!(
            counts.iter().map(|count| &count.path).collect::<Vec<_>>(),
            roots.iter().collect::<Vec<_>>()
        );
        assert!(store.library_root_counts(&[])?.is_empty());
        assert!(
            store
                .library_root_counts(&vec![
                    PathBuf::from("/music");
                    crate::model::MAX_QUERY_ROWS + 1
                ])
                .is_err()
        );
        Ok(())
    }
}
