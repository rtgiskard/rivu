use super::positions::close_gaps;
use super::rows::{
    LIBRARY_ROW_COLUMNS, LIBRARY_ROW_COLUMNS_QUALIFIED, TRACK_COLUMNS, library_row_at,
    library_row_from_row, track_from_row,
};
use super::{Store, batch_limit};
use anyhow::{Context, Result, bail};
use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::model::{Batch, HistoryEntry, Page, Track};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const TRACK_SEARCH_FILTER: &str = "WHERE (?1 IS NULL OR instr(lower(COALESCE(tracks.title_override,tracks.raw_title) || ' ' || COALESCE(tracks.artist_override,tracks.raw_artist) || ' ' || tracks.path),?1)>0) AND (?2 IS NULL OR tracks.favorite=?2) AND (?3 IS NULL OR tracks.missing=?3)";
pub(super) const SOURCE_FILTER: &str = "tracks.id IN (SELECT value FROM json_each(?2) UNION SELECT scoped.id FROM json_each(?1,'$.directories') AS directory CROSS JOIN tracks AS scoped INDEXED BY tracks_path WHERE directory.value<>'' AND scoped.path>=directory.value AND scoped.path<(rtrim(directory.value,'/')||'0') UNION SELECT id FROM tracks WHERE EXISTS(SELECT 1 FROM json_each(?1,'$.directories') WHERE value='') UNION SELECT id FROM tracks WHERE json_type(?1,'$.roots')='array' AND NOT EXISTS(SELECT 1 FROM json_each(?1,'$.roots') AS root WHERE tracks.path>=root.value AND tracks.path<(rtrim(root.value,'/')||'0')))";
// Normalized nonempty prefixes end in ASCII '/'; replacing that final byte with
// '0' gives the exclusive BINARY upper bound, including every Unicode suffix.
const DIRECTORY_SCOPE: &str = " AND tracks.path>=?4 AND tracks.path<(rtrim(?4,'/')||'0')";
const OTHER_SCOPE: &str = " AND NOT EXISTS(SELECT 1 FROM json_each(?4) AS root WHERE tracks.path>=root.value AND tracks.path<(rtrim(root.value,'/')||'0'))";
const DIRECTORY_CHILDREN: &str = "(SELECT 0 AS kind,CASE WHEN ?4='' AND instr(rest,'/')=1 THEN '/' ELSE ?4||substr(rest,1,instr(rest,'/')-1) END AS child,MIN(id) AS track_id FROM sources WHERE instr(rest,'/')>0 GROUP BY child UNION ALL SELECT 1,path,id FROM sources WHERE instr(rest,'/')=0)";

fn bool_key(value: Option<bool>) -> &'static str {
    match value {
        None => "",
        Some(false) => "0",
        Some(true) => "1",
    }
}

/// SQL literal substring matching, shared by occurrence and album projections.
/// Query normalization follows library search; '%' and '_' are not wildcards.
pub(super) fn search_query(query: Option<&str>) -> Option<String> {
    query
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase)
}

pub(super) fn search_filter(table: &str, parameter: &str) -> String {
    format!(
        "({parameter} IS NULL OR instr(lower(COALESCE({table}.title_override,{table}.raw_title) || ' ' || COALESCE({table}.artist_override,{table}.raw_artist) || ' ' || COALESCE({table}.album_override,{table}.raw_album) || ' ' || substr({table}.path,length(rtrim({table}.path,replace({table}.path,'/','')))+1)),{parameter})>0)"
    )
}

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

fn other_root_prefixes(roots: &[PathBuf]) -> Result<Vec<String>> {
    crate::library::normalize_roots(roots)?
        .iter()
        .map(|root| {
            let root = root.to_str().context("Library root path is not UTF-8")?;
            Ok(format!("{}/", root.trim_end_matches('/')))
        })
        .collect()
}

fn library_order(sort: rivu_protocol::model::LibrarySort) -> &'static str {
    match sort {
        rivu_protocol::model::LibrarySort::Id => "id",
        rivu_protocol::model::LibrarySort::Album => {
            "COALESCE(album_override,raw_album), CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(disc_number,4294967295) END, CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(track_number,4294967295) END, COALESCE(title_override,raw_title),id"
        }
        rivu_protocol::model::LibrarySort::MostPlayed => "play_count DESC,id",
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
    pub queue: bool,
    pub albums: bool,
    pub playlists: Vec<i64>,
    pub duplicate_groups: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackEditImpact {
    pub changed: bool,
    pub library: bool,
    pub queue: bool,
    pub albums: bool,
    pub member_facts: bool,
    /// None means the changed playlist scopes exceeded the publication bound.
    pub playlists: Option<Vec<i64>>,
}

#[derive(Debug, Default)]
pub struct FavoriteImpact {
    pub changed: bool,
    pub queue: bool,
    pub albums: bool,
    pub tracks: Option<Vec<i64>>,
    pub playlists: Option<Vec<i64>>,
}

/// Captured before loading audio; bitrate persistence must still match this source.
pub(crate) struct PlaybackSource {
    pub(crate) id: i64,
    pub(crate) path: PathBuf,
    pub(crate) file_size: u64,
    pub(crate) modified_ns: i64,
    cue: Option<rivu_protocol::model::CueSegment>,
    fingerprint: Option<blake3::Hash>,
}

// Only the latest scope is retained; no row payloads or per-query map accumulate.
pub(super) struct TrackCount {
    changes: u64,
    query: Option<String>,
    favorite: Option<bool>,
    missing: Option<bool>,
    prefix: Option<String>,
    total: usize,
}

impl Store {
    pub(crate) fn other_tracks_sources(&self, roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let roots = serde_json::to_string(&other_root_prefixes(roots)?)?;
        let mut statement = self.conn.prepare(&format!(
            "SELECT DISTINCT path FROM tracks WHERE 1=1{} ORDER BY path",
            OTHER_SCOPE.replace("?4", "?1")
        ))?;
        Ok(statement
            .query_map([roots], |row| Ok(PathBuf::from(row.get::<_, String>(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn other_tracks_count(&self, roots: &[PathBuf]) -> Result<usize> {
        let roots = serde_json::to_string(&other_root_prefixes(roots)?)?;
        self.cached_count(super::CountDomain::Other, &[&roots, "", "", ""], || {
            Ok(self.conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM tracks WHERE 1=1{}",
                    OTHER_SCOPE.replace("?4", "?1")
                ),
                [&roots],
                |row| row.get::<_, i64>(0),
            )? as usize)
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The Other projection composes the existing library filter and page contract"
    )]
    pub(crate) fn other_tracks_page(
        &self,
        roots: &[PathBuf],
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: rivu_protocol::model::LibrarySort,
        offset: usize,
        limit: usize,
    ) -> Result<Page<rivu_protocol::model::LibraryRow>> {
        let roots = serde_json::to_string(&other_root_prefixes(roots)?)?;
        let query = search_query(query);
        let total = self.cached_count(
            super::CountDomain::Other,
            &[
                &roots,
                query.as_deref().unwrap_or(""),
                bool_key(favorite),
                bool_key(missing),
            ],
            || {
                Ok(self.conn.query_row(
                    &format!("SELECT COUNT(*) FROM tracks {TRACK_SEARCH_FILTER}{OTHER_SCOPE}"),
                    params![query, favorite, missing, roots],
                    |row| row.get::<_, i64>(0),
                )? as usize)
            },
        )?;
        let mut rows = Vec::new();
        if limit != 0 && offset < total {
            let mut statement = self.conn.prepare(&format!(
                "SELECT {LIBRARY_ROW_COLUMNS} FROM tracks {TRACK_SEARCH_FILTER}{OTHER_SCOPE} ORDER BY {} LIMIT ?5 OFFSET ?6",
                library_order(sort)
            ))?;
            rows = statement
                .query_map(
                    params![
                        query,
                        favorite,
                        missing,
                        roots,
                        limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64,
                        offset.min(i64::MAX as usize) as i64
                    ],
                    library_row_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(Page { total, rows })
    }

    pub(crate) fn other_tracks_location(
        &self,
        roots: &[PathBuf],
        track_id: i64,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: rivu_protocol::model::LibrarySort,
    ) -> Result<rivu_protocol::model::LibraryLocateResult> {
        use rivu_protocol::model::{LibraryLocateResult, LibraryLocation};
        let roots = serde_json::to_string(&other_root_prefixes(roots)?)?;
        let exists = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
            [track_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Ok(LibraryLocateResult::Unavailable);
        }
        let in_scope = self.conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1{})",
                OTHER_SCOPE.replace("?4", "?2")
            ),
            params![track_id, roots],
            |row| row.get::<_, bool>(0),
        )?;
        if !in_scope {
            return Ok(LibraryLocateResult::OutsideRoot);
        }
        let query = search_query(query);
        let location = self.conn.query_row(&format!(
            "WITH ranked AS (SELECT id,row_number() OVER (ORDER BY {})-1 AS position,COUNT(*) OVER () AS total FROM tracks {TRACK_SEARCH_FILTER}{OTHER_SCOPE}) SELECT position,total FROM ranked WHERE id=?5", library_order(sort)
        ), params![query, favorite, missing, roots, track_id], |row| {
            Ok(LibraryLocation { offset: row.get::<_, i64>(0)? as usize,
                total: row.get::<_, i64>(1)? as usize, child: None })
        }).optional()?;
        Ok(location.map_or(LibraryLocateResult::Filtered, LibraryLocateResult::Located))
    }

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
        let tx = self.conn.unchecked_transaction()?;
        let updated = tx.execute(
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
        tx.commit()?;
        if updated != 0
            && let Some((track, _)) = self.track_cache.borrow_mut().entries.get_mut(&source.id)
        {
            track.bitrate_bps = Some(bitrate_bps);
            track.bitrate_exact = bitrate_exact;
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
        sort: rivu_protocol::model::LibrarySort,
        offset: usize,
        limit: usize,
    ) -> Result<Page<rivu_protocol::model::LibraryRow>> {
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
                    limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64,
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
        sort: rivu_protocol::model::LibrarySort,
        offset: usize,
        limit: usize,
    ) -> Result<Page<rivu_protocol::model::LibraryRow>> {
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
                    limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64,
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
        sort: rivu_protocol::model::LibrarySort,
    ) -> Result<rivu_protocol::model::LibraryLocateResult> {
        use rivu_protocol::model::{LibraryLocateResult, LibraryLocation};
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
        let scope = if prefix.is_empty() {
            ""
        } else {
            DIRECTORY_SCOPE
        };
        let child = if tree {
            rest.find('/').map(|separator| {
                if prefix.is_empty() && separator == 0 {
                    PathBuf::from("/")
                } else {
                    PathBuf::from(format!("{}{}", prefix, &rest[..separator]))
                }
            })
        } else {
            None
        };
        let (sql, target) = if tree {
            (
                format!(
                    "WITH sources AS (SELECT id,path,substr(path,length(?4)+1) AS rest FROM tracks {TRACK_SEARCH_FILTER}{scope}), children AS {DIRECTORY_CHILDREN}, ranked AS (SELECT kind,child,track_id,row_number() OVER (ORDER BY kind,child,track_id)-1 AS position,COUNT(*) OVER () AS total FROM children) SELECT position,total FROM ranked WHERE (?5 IS NOT NULL AND kind=0 AND child=?5) OR (?5 IS NULL AND kind=1 AND track_id=?6)"
                ),
                child.as_deref().and_then(Path::to_str),
            )
        } else {
            (
                format!(
                    "WITH ranked AS (SELECT id,row_number() OVER (ORDER BY {})-1 AS position,COUNT(*) OVER () AS total FROM tracks {TRACK_SEARCH_FILTER}{scope}) SELECT position,total FROM ranked WHERE id=?6 AND ?5 IS NULL",
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
    ) -> Result<Vec<rivu_protocol::model::LibraryRootCount>> {
        if roots.len() > rivu_protocol::model::MAX_QUERY_ROWS {
            bail!(
                "Library root count request exceeds {} roots",
                rivu_protocol::model::MAX_QUERY_ROWS
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
            .map(|(path, prefix)| rivu_protocol::model::LibraryRootCount {
                path: path.clone(),
                total: totals[indices[prefix.as_str()]],
            })
            .collect())
    }

    /// Count catalog tracks recursively and count each source path's cached bytes once.
    /// Missing tracks retain their last scanned size; no filesystem access is needed.
    pub fn library_root_stats(
        &self,
        roots: &[PathBuf],
    ) -> Result<Vec<rivu_protocol::model::LibraryRootStats>> {
        anyhow::ensure!(
            roots.len() <= rivu_protocol::model::MAX_QUERY_ROWS,
            "Library root stats request exceeds {} roots",
            rivu_protocol::model::MAX_QUERY_ROWS
        );
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let prefixes = roots
            .iter()
            .map(|path| directory_prefix(path))
            .collect::<Result<Vec<_>>>()?;
        let mut statement = self.conn.prepare(
            "WITH roots AS (SELECT key,value FROM json_each(?1)),
             source_totals AS (
                 SELECT roots.key AS root_key,COUNT(*) AS track_total,MAX(tracks.file_size) AS source_size
                 FROM roots CROSS JOIN tracks INDEXED BY tracks_path
                 WHERE roots.value<>'' AND tracks.path>=roots.value
                   AND tracks.path<(rtrim(roots.value,'/')||'0')
                 GROUP BY roots.key,tracks.path
                 UNION ALL
                 SELECT roots.key,COUNT(*),MAX(tracks.file_size)
                 FROM roots CROSS JOIN tracks INDEXED BY tracks_path
                 WHERE roots.value='' GROUP BY roots.key,tracks.path
             )
             SELECT roots.key,COALESCE(SUM(track_total),0),COALESCE(SUM(source_size),0)
             FROM roots LEFT JOIN source_totals ON source_totals.root_key=roots.key
             GROUP BY roots.key ORDER BY roots.key",
        )?;
        Ok(statement
            .query_map([serde_json::to_string(&prefixes)?], |row| {
                Ok(rivu_protocol::model::LibraryRootStats {
                    path: roots[row.get::<_, i64>(0)? as usize].clone(),
                    total: row.get::<_, i64>(1)? as usize,
                    file_size: row.get::<_, i64>(2)? as u64,
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
        // Writable transactions bypass counts; read-only owners fence invalidation.
        let cacheable = self.revision_owned_counts || self.conn.is_autocommit();
        let changes = self.conn.total_changes();
        if cacheable
            && let Some(cached) = self.count_cache.borrow().as_ref()
            && cached.changes == changes
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
                query: query.map(str::to_owned),
                favorite,
                missing,
                prefix: prefix.map(str::to_owned),
                total,
            });
        }
        Ok(total)
    }

    pub fn library_stats(&self) -> Result<rivu_protocol::model::LibraryStats> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*),COALESCE(SUM(play_count),0) FROM tracks",
            [],
            |row| {
                Ok(rivu_protocol::model::LibraryStats {
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
    ) -> Result<Page<rivu_protocol::model::DirectoryRow>> {
        use rivu_protocol::model::DirectoryRow;
        let prefix = directory_prefix(path)?;
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let scope = if prefix.is_empty() {
            ""
        } else {
            DIRECTORY_SCOPE
        };
        // Group narrow children, preserving CUE identity; only the bounded page
        // joins track payloads. The exact scalar total survives page changes.
        let children = format!(
            "WITH sources AS (SELECT id,path,substr(path,length(?4)+1) AS rest FROM tracks {TRACK_SEARCH_FILTER}{scope}), children AS {DIRECTORY_CHILDREN}"
        );
        let total = self.cached_count(
            super::CountDomain::Directory,
            &[
                &prefix,
                query.as_deref().unwrap_or(""),
                bool_key(favorite),
                bool_key(missing),
            ],
            || {
                Ok(self.conn.query_row(
                    &format!("{children} SELECT COUNT(*) FROM children"),
                    params![query, favorite, missing, prefix],
                    |row| row.get::<_, i64>(0),
                )? as usize)
            },
        )?;
        if limit == 0 || offset >= total {
            return Ok(Page {
                total,
                rows: Vec::new(),
            });
        }
        let sql = format!(
            "{children}, page AS MATERIALIZED (SELECT kind,child,track_id FROM children ORDER BY kind,child,track_id LIMIT ?5 OFFSET ?6) SELECT p.kind,p.child,{LIBRARY_ROW_COLUMNS_QUALIFIED} FROM page p LEFT JOIN tracks t ON p.kind=1 AND t.id=p.track_id ORDER BY p.kind,p.child,p.track_id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut result = stmt.query(params![
            query,
            favorite,
            missing,
            prefix,
            limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64,
            offset.min(i64::MAX as usize) as i64
        ])?;
        let mut page = Page {
            total,
            rows: Vec::new(),
        };
        while let Some(row) = result.next()? {
            let kind = row.get::<_, i64>(0)?;
            page.rows.push(if kind == 0 {
                DirectoryRow::Directory {
                    path: PathBuf::from(row.get::<_, String>(1)?),
                }
            } else {
                DirectoryRow::Track(library_row_at(row, 2)?)
            });
        }
        Ok(page)
    }

    pub(super) fn source_parameters(
        &self,
        directories: &[PathBuf],
        track_ids: &[i64],
        other_roots: Option<&[PathBuf]>,
    ) -> Result<(String, String)> {
        let ids = self.validated_track_ids(track_ids)?;
        let prefixes = directories
            .iter()
            .map(|path| directory_prefix(path))
            .collect::<Result<Vec<_>>>()?;
        let roots = other_roots.map(other_root_prefixes).transpose()?;
        #[derive(serde::Serialize)]
        struct Scope {
            directories: Vec<String>,
            roots: Option<Vec<String>>,
        }
        Ok((
            serde_json::to_string(&Scope {
                directories: prefixes,
                roots,
            })?,
            ids,
        ))
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

    pub fn track_id_for_identity(&self, identity: super::TrackIdentity<'_>) -> Result<Option<i64>> {
        let id = match identity {
            super::TrackIdentity::File(path) => self
                .conn
                .query_row(
                    "SELECT id FROM tracks WHERE path=?1 AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END = 0 AND missing=0",
                    [path.to_string_lossy().as_ref()],
                    |row| row.get(0),
                )
                .optional()?,
            super::TrackIdentity::Cue(path, number) => self
                .conn
                .query_row(
                    "SELECT id FROM tracks WHERE cue_sheet=?1 AND cue_number=?2 AND missing=0",
                    params![path.to_string_lossy().as_ref(), number],
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
        let limit = limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64;
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
        impact.queue = playlist_values_changed
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=0 AND track_id=?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )?;
        impact.albums = tx.query_row(
            &format!(
                "SELECT ((?1 AND ({} OR {})) OR (?2 AND {})) FROM tracks AS t WHERE id=?3",
                super::albums::eligible_member("t"),
                super::albums::eligible_member("t").replace(
                    "COALESCE(t.album_override,t.raw_album)",
                    "COALESCE(?4,t.album_override,t.raw_album)"
                ),
                super::albums::representative("t")
            ),
            params![
                playlist_values_changed,
                impact.library && !playlist_values_changed,
                id,
                album
            ],
            |row| row.get(0),
        )?;
        if impact.library {
            impact.playlists = if playlist_values_changed {
                bounded_metadata_ids(
                    &tx,
                    "SELECT DISTINCT playlist_id FROM playlist_entries WHERE playlist_id>0 AND track_id=?1",
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
    #[cfg(test)]
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
        let albums = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM tracks AS t WHERE id IN (SELECT value FROM json_each(?1)) AND {})", super::albums::eligible_member("t")), [&removal.encoded], |row| row.get(0))?;
        let (playlists, queue) = self.remove_track_rows(&tx, removal)?;
        tx.commit()?;
        Ok(TrackRemovalImpact {
            removed,
            queue,
            albums,
            playlists,
            duplicate_groups,
        })
    }

    fn remove_track_rows(
        &self,
        tx: &Transaction<'_>,
        removal: PreparedTrackRemoval<'_>,
    ) -> Result<(Vec<i64>, bool)> {
        for id in removal.ids {
            self.track_cache.borrow_mut().entries.remove(id);
        }
        let (_, playlists, queue) = remove_track_references(tx, Some(&removal.encoded))?;
        Ok((playlists, queue))
    }

    pub fn remove_missing_tracks(&self) -> Result<TrackRemovalImpact> {
        self.track_cache.borrow_mut().entries.clear();
        let tx = self.conn.unchecked_transaction()?;
        let duplicate_groups = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE missing=1 AND cue_sheet IS NULL AND fingerprint IS NOT NULL)",
            [], |row| row.get(0),
        )?;
        let albums = tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM tracks AS t WHERE missing=1 AND {})",
                super::albums::eligible_member("t")
            ),
            [],
            |row| row.get(0),
        )?;
        let (removed, playlists, queue) = remove_track_references(&tx, None)?;
        tx.commit()?;
        Ok(TrackRemovalImpact {
            removed,
            queue,
            albums,
            playlists,
            duplicate_groups,
        })
    }
    pub fn mark_played(&self, track: i64, at: i64) -> Result<super::TrackStats> {
        let tx = self.conn.unchecked_transaction()?;
        let (mut stats, changed) = match tx.query_row(
            "UPDATE tracks SET last_played=?1 WHERE id=?2 AND last_played IS NOT ?1 RETURNING play_count,last_played",
            params![at, track], super::TrackStats::from_row,
        ).optional()? {
            Some(stats) => (stats, true),
            None => (tx.query_row("SELECT play_count,last_played FROM tracks WHERE id=?1", [track], super::TrackStats::from_row)
                .optional()?.with_context(|| format!("track {track} does not exist"))?, false),
        };
        stats.changed = changed;
        stats.albums = changed
            && tx.query_row(
                &format!(
                    "SELECT {} FROM tracks AS t WHERE id=?1",
                    super::albums::representative("t")
                ),
                [track],
                |row| row.get::<_, bool>(0),
            )?;
        tx.commit()?;
        self.cache_track_stats(track, stats);
        Ok(stats)
    }
    pub fn increment_play_count(&self, track: i64) -> Result<super::TrackStats> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stats = tx.query_row(
            "UPDATE tracks SET play_count=play_count+1 WHERE id=?1 RETURNING play_count,last_played",
            [track], super::TrackStats::from_row,
        ).optional()?.with_context(|| format!("track {track} does not exist"))?;
        stats.changed = true;
        (stats.queue, stats.albums) = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=0 AND track_id=t.id),{} FROM tracks AS t WHERE id=?1", super::albums::eligible_member("t")), [track], |row| Ok((row.get(0)?, row.get(1)?)))?;
        tx.commit()?;
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
        .take(rivu_protocol::model::MAX_METADATA_SCOPES + 1)
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok((ids.len() <= rivu_protocol::model::MAX_METADATA_SCOPES).then_some(ids))
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
            "SELECT DISTINCT p.playlist_id FROM tracks AS t JOIN playlist_entries AS p ON p.track_id=t.id WHERE p.playlist_id>0 AND t.id IN (SELECT value FROM json_each(?1)) AND (?2 IS NULL OR t.favorite<>?2)",
            params![encoded_ids, favorite],
        )?,
        None => None,
    };
    let (queue, albums) = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM tracks AS t JOIN playlist_entries AS q ON q.track_id=t.id WHERE q.playlist_id=0 AND t.id IN (SELECT value FROM json_each(?1)) AND (?2 IS NULL OR t.favorite<>?2)), EXISTS(SELECT 1 FROM tracks AS t WHERE t.id IN (SELECT value FROM json_each(?1)) AND (?2 IS NULL OR t.favorite<>?2) AND {})", super::albums::eligible_member("t")),
        params![encoded_ids, favorite], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(FavoriteImpact {
        queue,
        albums,
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

/// Select IDs in SQL so missing cleanup never materializes the catalog in Rust.
fn remove_track_references(
    tx: &Transaction<'_>,
    encoded_ids: Option<&str>,
) -> Result<(usize, Vec<i64>, bool)> {
    // Keep selection shared across all statements without allocating SQL strings.
    macro_rules! queries {
        ($ids:literal) => {
            (
                concat!("SELECT position FROM playlist_entries WHERE playlist_id=0 AND track_id IN (", $ids, ") ORDER BY position"),
                concat!("SELECT playlist_id,position FROM playlist_entries WHERE playlist_id>0 AND track_id IN (", $ids, ") ORDER BY playlist_id,position"),
                concat!("DELETE FROM playlist_entries WHERE track_id IN (", $ids, ")"),
                concat!("DELETE FROM tracks WHERE id IN (", $ids, ")"),
            )
        };
    }
    let (queue_sql, playlists_sql, entries_sql, tracks_sql) = if encoded_ids.is_some() {
        queries!("SELECT value FROM json_each(?1)")
    } else {
        queries!("SELECT id FROM tracks WHERE missing=1")
    };
    let parameters = params![encoded_ids];
    let parameters = if encoded_ids.is_some() {
        parameters
    } else {
        &[]
    };
    let queue_positions = tx
        .prepare(queue_sql)?
        .query_map(parameters, |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    let playlist_positions = removed_playlist_positions(tx, playlists_sql, parameters)?;
    tx.execute(entries_sql, parameters)?;
    let removed = tx.execute(tracks_sql, parameters)?;
    close_gaps(tx, &queue_positions, 0)?;
    let mut playlists = Vec::with_capacity(playlist_positions.len());
    for (playlist_id, positions) in playlist_positions {
        close_gaps(tx, &positions, playlist_id)?;
        playlists.push(playlist_id);
    }
    Ok((removed, playlists, !queue_positions.is_empty()))
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
             INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES
                (100,0,1,0),(101,0,2,1),(102,0,2,2),(103,0,3,3),
                (104,0,4,4),(105,0,5,5),(106,0,6,6),(107,0,7,7);
             UPDATE rivu_meta SET current_queue_entry_id=106 WHERE singleton=1;
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
            .prepare("SELECT id,track_id,position FROM playlist_entries WHERE playlist_id=0 ORDER BY position")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn playlist_rows(store: &Store) -> Result<Vec<(i64, i64, i64, i64)>> {
        Ok(store.conn.prepare("SELECT id,playlist_id,track_id,position FROM playlist_entries WHERE playlist_id>0 ORDER BY playlist_id,position")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn current(store: &Store) -> Result<Option<i64>> {
        Ok(store.conn.query_row(
            "SELECT current_queue_entry_id FROM rivu_meta WHERE singleton=1",
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
             UPDATE rivu_meta SET current_queue_entry_id=102 WHERE singleton=1;",
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
        assert!(store.remove_tracks(&[2, 999, 2]).is_err());
        store.remove_tracks(&[])?;
        store.remove_tracks(&[8, 8])?;
        assert_eq!(queue_rows(&store)?, queue);
        assert_eq!(playlist_rows(&store)?, playlists);
        assert_eq!(current(&store)?, Some(106));
        let track_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))?;
        assert_eq!(track_count, 7);
        Ok(())
    }

    #[test]
    fn both_deletion_selections_report_references_and_metadata_domains() -> Result<()> {
        for missing in [false, true] {
            let store = fixture()?;
            store.conn.execute_batch(
                "UPDATE tracks SET raw_album='Album' WHERE id=2;
                 UPDATE tracks SET fingerprint=zeroblob(32) WHERE id IN (2,3);
                 UPDATE tracks SET missing=1 WHERE id IN (2,8);",
            )?;
            let impact = if missing {
                store.remove_missing_tracks()?
            } else {
                store.remove_prepared_tracks(store.prepare_track_removal(&[2, 8])?)?
            };
            assert_eq!(impact.removed, 2);
            assert!(impact.queue && impact.albums && impact.duplicate_groups);
            assert_eq!(impact.playlists, [10, 20]);
            assert_eq!(current(&store)?, Some(106));
            assert!(store.track(2)?.is_none());
            assert!(store.track(8)?.is_none());
            assert!(store.track(3)?.is_some());
        }
        Ok(())
    }

    #[test]
    fn deletion_failure_restores_references_positions_and_current_occurrence() -> Result<()> {
        for missing in [false, true] {
            let store = fixture()?;
            store.conn.execute_batch(
                "UPDATE tracks SET missing=1 WHERE id=2;
                 CREATE TEMP TRIGGER reject_track_delete BEFORE DELETE ON tracks
                 WHEN OLD.id=2 BEGIN SELECT RAISE(ABORT,'reject deletion'); END;",
            )?;
            let queue = queue_rows(&store)?;
            let playlists = playlist_rows(&store)?;
            let result = if missing {
                store.remove_missing_tracks().map(|_| ())
            } else {
                store
                    .remove_prepared_tracks(store.prepare_track_removal(&[2])?)
                    .map(|_| ())
            };
            assert!(result.is_err());
            assert_eq!(queue_rows(&store)?, queue);
            assert_eq!(playlist_rows(&store)?, playlists);
            assert_eq!(current(&store)?, Some(106));
            assert!(store.track(2)?.is_some());
        }
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
    fn metadata_effects_distinguish_album_members_representatives_and_unqueued_tracks() -> Result<()>
    {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path,raw_title,raw_album,track_number) VALUES
             (1,'/album/one.wav','One','Album',1),
             (2,'/album/two.wav','Two','Album',2),
             (3,'/other/three.wav','Three','',NULL);",
        )?;
        store.enqueue_tracks(&[3])?;
        let playlist = store.create_playlist_with_tracks("Members", &[1, 2, 3])?;
        let member = store.edit_track(2, None, None, None, Some(DecoderHint::Native))?;
        assert!(member.changed && member.library && !member.queue && !member.albums);
        assert_eq!(member.playlists, Some(Vec::new()));
        let representative = store.edit_track(1, None, None, None, Some(DecoderHint::Native))?;
        assert!(representative.library && representative.albums && !representative.queue);
        let display = store.edit_track(2, Some("Retitled"), None, None, None)?;
        assert!(display.library && display.albums && !display.queue);
        assert_eq!(display.playlists, Some(vec![playlist]));
        let plain = store.set_favorite(&[3], true)?;
        assert!(plain.changed && plain.queue && !plain.albums);
        let album = store.set_favorite(&[2], true)?;
        assert!(album.changed && album.albums && !album.queue);
        let noop = store.set_favorite(&[2], true)?;
        assert!(!noop.changed && !noop.queue && !noop.albums);
        let removal = store.prepare_track_removal(&[2])?;
        let removed = store.remove_prepared_tracks(removal)?;
        assert_eq!(removed.removed, 1);
        assert!(removed.albums && !removed.queue);
        assert_eq!(removed.playlists, [playlist]);
        Ok(())
    }

    #[test]
    fn statistics_return_committed_changes_and_representative_last_played_effects() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        store.conn.execute_batch(
            "INSERT INTO tracks(id,path,raw_album,track_number) VALUES
             (1,'/album/one.wav','Album',1),(2,'/album/two.wav','Album',2),
             (3,'/plain/three.wav','',NULL);",
        )?;
        store.enqueue_tracks(&[3])?;
        let first = store.mark_played(1, 42)?;
        assert!(first.changed && first.albums && !first.queue);
        let noop = store.mark_played(1, 42)?;
        assert!(!noop.changed && !noop.albums && !noop.queue);
        let member = store.mark_played(2, 42)?;
        assert!(member.changed && !member.albums && !member.queue);
        let album_count = store.increment_play_count(2)?;
        assert!(album_count.changed && album_count.albums && !album_count.queue);
        let queue_count = store.increment_play_count(3)?;
        assert!(queue_count.changed && queue_count.queue && !queue_count.albums);
        let before = store.track(3)?.unwrap();
        store.conn.execute_batch("CREATE TEMP TRIGGER reject_statistics AFTER UPDATE OF play_count,last_played ON tracks BEGIN SELECT RAISE(ABORT,'reject statistics'); END;")?;
        assert!(store.increment_play_count(3).is_err());
        assert!(store.mark_played(3, 99).is_err());
        assert_eq!(store.track(3)?.unwrap(), before);
        let stored: (i64, Option<i64>) = store.conn.query_row(
            "SELECT play_count,last_played FROM tracks WHERE id=3",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(stored, (1, None));
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
            assert!(store.source_parameters(&[], ids, None).is_err());
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
        use rivu_protocol::model::DirectoryRow;
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
        use rivu_protocol::model::{DirectoryRow, LibrarySort};
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
                None,
            )?;
            let queued = store.queue_page(None, queue_before, 10)?;
            assert_eq!(
                queued
                    .rows
                    .iter()
                    .map(|row| row.track_id)
                    .collect::<Vec<_>>(),
                [included]
            );
            let playlist = store.create_playlist(root)?;
            store.add_playlist_sources(playlist, &[path.to_path_buf()], &[included], None)?;
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
        store.add_playlist_sources(playlist, &[PathBuf::new(), "/Upper".into()], &[1, 1], None)?;
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
                    rivu_protocol::model::MAX_QUERY_ROWS + 1
                ])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn library_root_stats_share_whole_and_cue_source_size_and_retain_missing_sizes() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO tracks(path,file_size,missing,cue_sheet,cue_number) VALUES
                 ('/music/image.flac',1024,0,NULL,NULL),
                 ('/music/image.flac',2048,1,'/sheets/album.cue',1),
                 ('/music/image.flac',2048,1,'/sheets/album.cue',2);",
            )
            .unwrap();
        let stats = store.library_root_stats(&["/music".into()]).unwrap();
        assert_eq!((stats[0].total, stats[0].file_size), (3, 2048));
    }

    #[test]
    fn library_root_stats_count_cue_tracks_and_unique_source_path_bytes() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        // Entirely catalog-backed: none of these paths need to exist.
        for (path, size, missing) in [
            ("/music/copy-a.flac", 1024, false),
            ("/music/deep/copy-b.flac", 1024, false),
            ("/music/missing.flac", 2048, true),
            ("/music2/outside.flac", 8192, false),
            ("/音楽/夏.flac", 4096, false),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO tracks(path,file_size,missing,fingerprint) VALUES(?1,?2,?3,zeroblob(32))",
                    params![path, size, missing],
                )
                .unwrap();
        }
        for (number, sheet) in [
            (1, "/sheets/album.cue"),
            (2, "/sheets/album.cue"),
            (1, "/other/album.cue"),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO tracks(path,file_size,cue_sheet,cue_number,cue_start_frame)
                     VALUES('/music/deep/album.flac',3072,?1,?2,?3)",
                    params![sheet, number, number * 75],
                )
                .unwrap();
        }
        let roots = [
            "/music",
            "/music/deep",
            "/music/./deep/..",
            "/music2",
            "/音楽",
            "/empty",
            "/music",
            "/",
            "",
        ]
        .map(PathBuf::from);
        let stats = store.library_root_stats(&roots).unwrap();
        assert_eq!(
            stats.iter().map(|stats| &stats.path).collect::<Vec<_>>(),
            roots.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            stats
                .iter()
                .map(|stats| (stats.total, stats.file_size))
                .collect::<Vec<_>>(),
            [
                (6, 7168),
                (4, 4096),
                (6, 7168),
                (1, 8192),
                (1, 4096),
                (0, 0),
                (6, 7168),
                (8, 19456),
                (8, 19456),
            ]
        );
        assert!(store.library_root_stats(&[]).unwrap().is_empty());
        assert!(
            store
                .library_root_stats(&vec![
                    PathBuf::from("/music");
                    rivu_protocol::model::MAX_QUERY_ROWS + 1
                ])
                .is_err()
        );
    }
}
