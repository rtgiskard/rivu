use crate::library::{KnownFile, MediaInfo, ScanResult};
use crate::model::{
    CueSegment, DatabaseOptimization, HistoryEntry, PlaylistEntryPage, PlaylistEntryRow,
    PlaylistSummary, Track, TrackPage,
};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// Bump when probing semantics change; fingerprints remain reusable.
const PROBE_VERSION: i64 = 3;

pub struct Store {
    conn: Connection,
    track_cache: RefCell<TrackCache>,
}

struct TrackCache {
    entries: HashMap<i64, (Track, u64)>,
    capacity: usize,
    clock: u64,
}

impl TrackCache {
    fn evict_oldest(&mut self) -> bool {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, (_, age))| *age)
            .map(|(&id, _)| id);
        oldest
            .map(|id| self.entries.remove(&id).is_some())
            .unwrap_or(false)
    }

    fn trim(&mut self) {
        while self.entries.len() > self.capacity {
            self.evict_oldest();
        }
    }
}
struct Existing {
    id: i64,
    path: PathBuf,
    hash: Option<String>,
    cue: Option<CueSegment>,
}

#[derive(Hash, PartialEq, Eq)]
enum TrackIdentity<'a> {
    File(&'a Path),
    Cue(&'a Path, u32),
}

fn identity<'a>(path: &'a Path, cue: Option<&'a CueSegment>) -> TrackIdentity<'a> {
    match cue {
        Some(cue) => TrackIdentity::Cue(&cue.sheet, cue.number),
        None => TrackIdentity::File(path),
    }
}

fn cue_from_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Option<CueSegment>> {
    let sheet: Option<String> = row.get(offset)?;
    sheet
        .map(|sheet| {
            Ok(CueSegment {
                sheet: sheet.into(),
                number: row.get(offset + 1)?,
                start_frame: {
                    let value: i64 = row.get(offset + 2)?;
                    u64::try_from(value)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(offset + 2, value))?
                },
                end_frame: row
                    .get::<_, Option<i64>>(offset + 3)?
                    .map(|value| {
                        u64::try_from(value).map_err(|_| {
                            rusqlite::Error::IntegralValueOutOfRange(offset + 3, value)
                        })
                    })
                    .transpose()?,
            })
        })
        .transpose()
}

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
        CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        INSERT INTO schema_meta(key,value) VALUES('version',6);",
    )?;
    tx.commit()?;
    Ok(())
}

fn track_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        fingerprint: r.get(22)?,
        title: r.get(2)?,
        artist: r.get(3)?,
        album: r.get(4)?,
        duration: r.get(5)?,
        codec: r.get(6)?,
        channels: r.get::<_, i64>(7)? as u16,
        sample_rate: r.get::<_, i64>(8)? as u32,
        bitrate_bps: r.get::<_, Option<i64>>(16)?.map(|value| value as u64),
        track_number: r.get(17)?,
        disc_number: r.get(18)?,
        bits_per_sample: r.get(19)?,
        release_date: r.get(20)?,
        favorite: r.get(21)?,
        missing: r.get::<_, i64>(9)? != 0,
        play_count: r.get::<_, i64>(10)? as u64,
        last_played: r.get(11)?,
        cue: cue_from_row(r, 12)?,
    })
}

const TRACK_COLUMNS: &str = "id,path,COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),COALESCE(album_override,raw_album),duration,codec,channels,sample_rate,missing,play_count,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame,bitrate_bps,track_number,disc_number,bits_per_sample,release_date,favorite,fingerprint";

const LIBRARY_ROW_COLUMNS: &str = "id,COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),COALESCE(album_override,raw_album),duration,favorite,missing,play_count";
const LIBRARY_ROW_COLUMNS_QUALIFIED: &str = "t.id,COALESCE(t.title_override,t.raw_title),COALESCE(t.artist_override,t.raw_artist),COALESCE(t.album_override,t.raw_album),t.duration,t.favorite,t.missing,t.play_count";
const SOURCE_FILTER: &str = "tracks.id IN (SELECT value FROM json_each(?2)) OR EXISTS (SELECT 1 FROM json_each(?1) AS directory WHERE substr(tracks.path,1,length(directory.value))=directory.value)";

fn library_row_at(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<crate::model::LibraryRow> {
    Ok(crate::model::LibraryRow {
        id: row.get(offset)?,
        title: row.get(offset + 1)?,
        artist: row.get(offset + 2)?,
        album: row.get(offset + 3)?,
        duration: row.get(offset + 4)?,
        favorite: row.get(offset + 5)?,
        missing: row.get(offset + 6)?,
        play_count: row.get::<_, i64>(offset + 7)? as u64,
    })
}

fn library_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<crate::model::LibraryRow> {
    library_row_at(row, 0)
}

fn directory_prefix(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(String::new());
    }
    let path = path
        .to_str()
        .context("Library directory path is not UTF-8")?;
    Ok(format!("{}/", path.trim_end_matches('/')))
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
            if version != 6 {
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
    pub(crate) fn set_track_cache_page_size(&self, page_size: u32) {
        let mut cache = self.track_cache.borrow_mut();
        cache.capacity = (page_size as usize).saturating_mul(2);
        cache.trim();
        let capacity = cache.capacity;
        cache.entries.shrink_to(capacity);
    }

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
    ) -> Result<crate::model::LibraryPage> {
        use crate::model::LibrarySort;
        let query = query
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let filter = "WHERE (?1 IS NULL OR instr(lower(COALESCE(title_override,raw_title)),?1)>0 OR instr(lower(COALESCE(artist_override,raw_artist)),?1)>0 OR instr(lower(COALESCE(album_override,raw_album)),?1)>0 OR instr(lower(path),?1)>0) AND (?2 IS NULL OR favorite=?2) AND (?3 IS NULL OR missing=?3)";
        let total = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks {filter}"),
            params![query, favorite, missing],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let order = match sort {
            LibrarySort::Id => "id",
            LibrarySort::Album => {
                "COALESCE(album_override,raw_album), CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(disc_number,4294967295) END, CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(track_number,4294967295) END, COALESCE(title_override,raw_title),id"
            }
            LibrarySort::MostPlayed => "play_count DESC,id",
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {LIBRARY_ROW_COLUMNS} FROM tracks {filter} ORDER BY {order} LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = stmt
            .query_map(
                params![
                    query,
                    favorite,
                    missing,
                    limit.min(crate::model::PAGE_SIZE) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                library_row_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(crate::model::LibraryPage { total, rows })
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

    pub fn playlist_summary_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<crate::model::PlaylistSummaryPage> {
        let total = self
            .conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |row| {
                row.get::<_, i64>(0)
            })? as usize;
        let rows = self.playlist_summaries(offset, limit.min(crate::model::PAGE_SIZE))?;
        Ok(crate::model::PlaylistSummaryPage { total, rows })
    }

    pub fn directory_page(
        &self,
        path: &Path,
        offset: usize,
        limit: usize,
    ) -> Result<crate::model::DirectoryPage> {
        use crate::model::{DirectoryPage, DirectoryRow};
        let prefix = directory_prefix(path)?;
        // SQLite derives immediate children; Rust retains only the requested page.
        // Group directories but preserve distinct CUE tracks sharing a source path.
        let children = "WITH sources AS (SELECT id,path,substr(path,length(?1)+1) AS rest FROM tracks WHERE substr(path,1,length(?1))=?1), children AS (SELECT 0 AS kind,CASE WHEN ?1='' AND instr(rest,'/')=1 THEN '/' ELSE ?1||substr(rest,1,instr(rest,'/')-1) END AS child,MIN(id) AS track_id FROM sources WHERE instr(rest,'/')>0 GROUP BY child UNION ALL SELECT 1,path,id FROM sources WHERE instr(rest,'/')=0)";
        let total = self.conn.query_row(
            &format!("{children} SELECT COUNT(*) FROM children"),
            [&prefix],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let mut stmt = self.conn.prepare(&format!("{children} SELECT c.kind,c.child,{LIBRARY_ROW_COLUMNS_QUALIFIED} FROM children c JOIN tracks t ON t.id=c.track_id ORDER BY c.kind,c.child,t.id LIMIT ?2 OFFSET ?3"))?;
        let rows = stmt
            .query_map(
                params![
                    prefix,
                    limit.min(crate::model::PAGE_SIZE) as i64,
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
        Ok(DirectoryPage { total, rows })
    }

    fn source_parameters(
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

    pub fn track(&self, id: i64) -> Result<Option<Track>> {
        let mut cache = self.track_cache.borrow_mut();
        if cache.clock == u64::MAX {
            cache.entries.clear();
            cache.clock = 0;
        }
        cache.clock += 1;
        let age = cache.clock;
        if let Some((track, touched)) = cache.entries.get_mut(&id) {
            *touched = age;
            return Ok(Some(track.clone()));
        }
        let sql = format!("SELECT {TRACK_COLUMNS} FROM tracks WHERE id=?1");
        let track = self.conn.query_row(&sql, [id], track_from_row).optional()?;
        if let Some(track) = &track
            && cache.capacity != 0
        {
            // Evict before insertion so entries never transiently exceed capacity.
            cache.trim();
            if cache.entries.len() == cache.capacity {
                cache.evict_oldest();
            }
            cache.entries.insert(id, (track.clone(), age));
        }
        Ok(track)
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
    ) -> Result<TrackPage> {
        let pattern = query
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);
        let where_sql = "WHERE (?1 IS NULL OR instr(lower(COALESCE(title_override,raw_title) || ' ' || COALESCE(artist_override,raw_artist) || ' ' || COALESCE(album_override,raw_album) || ' ' || path),?1)>0) AND (?2 IS NULL OR favorite=?2) AND (?3 IS NULL OR missing=?3)";
        let total_sql = format!("SELECT COUNT(*) FROM tracks {where_sql}");
        let total: i64 =
            self.conn
                .query_row(&total_sql, params![pattern, favorite, missing], |r| {
                    r.get(0)
                })?;
        let sql = format!(
            "SELECT {TRACK_COLUMNS} FROM tracks {where_sql} ORDER BY id LIMIT ?4 OFFSET ?5"
        );
        let limit = limit.min(crate::model::PAGE_SIZE) as i64;
        let offset = offset.min(i64::MAX as usize) as i64;
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![pattern, favorite, missing, limit, offset],
                track_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(TrackPage {
            total: total as usize,
            rows,
        })
    }

    pub fn playlist_summaries(&self, offset: usize, limit: usize) -> Result<Vec<PlaylistSummary>> {
        let mut stmt = self.conn.prepare("SELECT p.id,p.name,COUNT(e.id) FROM playlists p LEFT JOIN playlist_entries e ON e.playlist_id=p.id GROUP BY p.id ORDER BY p.id LIMIT ?1 OFFSET ?2")?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let offset = offset.min(i64::MAX as usize) as i64;
        Ok(stmt
            .query_map(params![limit, offset], |r| {
                Ok(PlaylistSummary {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    entry_count: r.get::<_, i64>(2)? as usize,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn playlist_entries_page(
        &self,
        playlist_id: i64,
        offset: usize,
        limit: usize,
    ) -> Result<PlaylistEntryPage> {
        let total: i64 = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1",
            [playlist_id],
            |r| r.get(0),
        ).optional()?.context("Playlist not found")?;
        let mut stmt = self.conn.prepare("SELECT e.id,e.track_id,COALESCE(t.title_override,t.raw_title),COALESCE(t.artist_override,t.raw_artist),COALESCE(t.album_override,t.raw_album),COALESCE(t.missing,1) FROM playlist_entries e LEFT JOIN tracks t ON t.id=e.track_id WHERE e.playlist_id=?1 ORDER BY e.position,e.id LIMIT ?2 OFFSET ?3")?;
        let limit = limit.min(crate::model::PAGE_SIZE) as i64;
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
        Ok(PlaylistEntryPage {
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

    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let mut q = self.conn.prepare(
            "SELECT id,COALESCE(title_override,raw_title),last_played FROM tracks
             WHERE last_played IS NOT NULL ORDER BY last_played DESC,id DESC LIMIT ?",
        )?;
        Ok(q.query_map([limit.min(i64::MAX as usize) as i64], |r| {
            Ok(HistoryEntry {
                track_id: r.get(0)?,
                title: r.get(1)?,
                played_at: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn known_files(&self) -> Result<Vec<KnownFile>> {
        let mut q=self.conn.prepare("SELECT id,path,file_size,modified_ns,fingerprint,raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,bits_per_sample,release_date FROM tracks")?;
        Ok(q.query_map([], |r| {
            Ok(KnownFile {
                track_id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                size: r.get::<_, i64>(2)? as u64,
                modified_ns: r.get(3)?,
                fingerprint: r.get(4)?,
                cue: cue_from_row(r, 12)?,
                media: if r.get::<_, i64>(16)? == PROBE_VERSION {
                    Some(MediaInfo {
                        title: r.get(5)?,
                        artist: r.get(6)?,
                        album: r.get(7)?,
                        duration: r.get(8)?,
                        codec: r.get(9)?,
                        channels: r.get::<_, i64>(10)? as u16,
                        sample_rate: r.get::<_, i64>(11)? as u32,
                        bitrate_bps: r.get::<_, Option<i64>>(17)?.map(|value| value as u64),
                        track_number: r.get(18)?,
                        disc_number: r.get(19)?,
                        bits_per_sample: r.get(20)?,
                        release_date: r.get(21)?,
                    })
                } else {
                    None
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn apply_scan(&self, result: &ScanResult) -> Result<()> {
        self.track_cache.borrow_mut().entries.clear();
        for r in &result.records {
            if r.path.to_str().is_none() {
                bail!("scan path is not UTF-8: {}", r.path.display());
            }
            if let Some(cue) = &r.cue {
                if cue.sheet.to_str().is_none() {
                    bail!("CUE sheet path is not UTF-8: {}", cue.sheet.display());
                }
                i64::try_from(cue.start_frame).context("CUE start frame exceeds database range")?;
                if let Some(end) = cue.end_frame {
                    i64::try_from(end).context("CUE end frame exceeds database range")?;
                }
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut q = tx.prepare("SELECT id,path,fingerprint,cue_sheet,cue_number,cue_start_frame,cue_end_frame FROM tracks")?;
        let old: Vec<Existing> = q
            .query_map([], |r| {
                Ok(Existing {
                    id: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    hash: r.get(2)?,
                    cue: cue_from_row(r, 3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        let identities: HashSet<TrackIdentity<'_>> = result
            .records
            .iter()
            .map(|r| identity(&r.path, r.cue.as_ref()))
            .collect();
        let suppressed: HashSet<&Path> = result
            .suppressed_sources
            .iter()
            .map(PathBuf::as_path)
            .collect();
        let candidate_hashes: HashSet<&str> = result
            .records
            .iter()
            .filter_map(|r| r.fingerprint.as_deref())
            .collect();
        let mut byidentity = HashMap::new();
        let mut byhash: HashMap<&str, Vec<&Existing>> = HashMap::new();
        for record in &old {
            let key = identity(&record.path, record.cue.as_ref());
            let owner = record
                .cue
                .as_ref()
                .map_or(record.path.as_path(), |cue| cue.sheet.as_path());
            if !identities.contains(&key)
                && let Some(hash) = record.hash.as_deref()
                && candidate_hashes.contains(hash)
                && fs::symlink_metadata(owner)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                byhash.entry(hash).or_default().push(record);
            }
            byidentity.insert(key, record.id);
        }
        let mut seen = HashSet::new();
        tx.execute_batch(
            "CREATE TEMP TABLE scan_stage(
                resolved_id INTEGER,
                path TEXT NOT NULL,
                fingerprint TEXT,
                file_size INTEGER NOT NULL,
                modified_ns INTEGER NOT NULL,
                raw_title TEXT NOT NULL,
                raw_artist TEXT NOT NULL,
                raw_album TEXT NOT NULL,
                duration REAL,
                codec TEXT NOT NULL,
                channels INTEGER NOT NULL,
                sample_rate INTEGER NOT NULL,
                cue_sheet TEXT,
                cue_number INTEGER,
                cue_start_frame INTEGER,
                cue_end_frame INTEGER,
                probe_version INTEGER NOT NULL,
                bitrate_bps INTEGER,
                track_number INTEGER,
                disc_number INTEGER,
                bits_per_sample INTEGER,
                release_date TEXT
            )",
        )?;
        let mut stage = tx.prepare(
            "INSERT INTO temp.scan_stage(
                resolved_id,path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,
                duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,
                cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,bits_per_sample,
                release_date
            ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )?;
        for r in &result.records {
            let key = identity(&r.path, r.cue.as_ref());
            let id = byidentity.get(&key).copied().or_else(|| {
                r.fingerprint.as_deref().and_then(|hash| {
                    let mut candidates =
                        byhash
                            .get(hash)?
                            .iter()
                            .filter(|old| match (&old.cue, &r.cue) {
                                (None, None) => true,
                                (Some(a), Some(b)) => {
                                    a.number == b.number
                                        && a.start_frame == b.start_frame
                                        && a.end_frame == b.end_frame
                                }
                                _ => false,
                            });
                    let candidate = candidates.next()?;
                    (candidates.next().is_none() && !seen.contains(&candidate.id))
                        .then_some(candidate.id)
                })
            });
            let m = &r.media;
            let sheet = r.cue.as_ref().and_then(|cue| cue.sheet.to_str());
            let number = r.cue.as_ref().map(|cue| cue.number);
            let start = r.cue.as_ref().map(|cue| cue.start_frame as i64);
            let end = r
                .cue
                .as_ref()
                .and_then(|cue| cue.end_frame)
                .map(|end| end as i64);
            let bitrate = m
                .bitrate_bps
                .map(i64::try_from)
                .transpose()
                .context("Audio bitrate exceeds database range")?;
            stage.execute(params![
                id,
                r.path.to_str(),
                r.fingerprint.as_deref(),
                r.size as i64,
                r.modified_ns,
                m.title,
                m.artist,
                m.album,
                m.duration,
                m.codec,
                m.channels as i64,
                m.sample_rate as i64,
                sheet,
                number,
                start,
                end,
                PROBE_VERSION,
                bitrate,
                m.track_number,
                m.disc_number,
                m.bits_per_sample,
                m.release_date,
            ])?;
            if let Some(id) = id {
                seen.insert(id);
            }
        }
        drop(stage);
        tx.execute(
            "UPDATE tracks SET
                path=s.path,fingerprint=s.fingerprint,file_size=s.file_size,modified_ns=s.modified_ns,
                raw_title=s.raw_title,raw_artist=s.raw_artist,raw_album=s.raw_album,duration=s.duration,
                codec=s.codec,channels=s.channels,sample_rate=s.sample_rate,cue_sheet=s.cue_sheet,
                cue_number=s.cue_number,cue_start_frame=s.cue_start_frame,cue_end_frame=s.cue_end_frame,
                missing=0,probe_version=s.probe_version,bitrate_bps=s.bitrate_bps,
                track_number=s.track_number,disc_number=s.disc_number,bits_per_sample=s.bits_per_sample,
                release_date=s.release_date
             FROM temp.scan_stage AS s WHERE tracks.id=s.resolved_id",
            [],
        )?;
        tx.execute(
            "INSERT INTO tracks(
                path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,duration,codec,
                channels,sample_rate,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,
                bitrate_bps,track_number,disc_number,bits_per_sample,release_date
             ) SELECT path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,duration,codec,
                channels,sample_rate,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,
                bitrate_bps,track_number,disc_number,bits_per_sample,release_date
             FROM temp.scan_stage WHERE resolved_id IS NULL",
            [],
        )?;
        let mut current_stmt = tx.prepare(
            "SELECT id,path,fingerprint,cue_sheet,cue_number,cue_start_frame,cue_end_frame FROM tracks",
        )?;
        let current: Vec<Existing> = current_stmt
            .query_map([], |r| {
                Ok(Existing {
                    id: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    hash: r.get(2)?,
                    cue: cue_from_row(r, 3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(current_stmt);
        let mut current_by_identity = HashMap::new();
        for record in &current {
            current_by_identity.insert(identity(&record.path, record.cue.as_ref()), record.id);
        }
        for r in &result.records {
            if let Some(id) = current_by_identity.get(&identity(&r.path, r.cue.as_ref())) {
                seen.insert(*id);
            }
        }
        tx.execute_batch("DROP TABLE temp.scan_stage")?;
        let mut mark_missing = tx.prepare("UPDATE tracks SET missing=1 WHERE id=?1")?;
        for o in &old {
            let owner = o
                .cue
                .as_ref()
                .map_or(o.path.as_path(), |cue| cue.sheet.as_path());
            if !seen.contains(&o.id)
                && !(o.cue.is_none() && suppressed.contains(o.path.as_path()))
                && result
                    .roots
                    .iter()
                    .any(|root| owner == root || owner.starts_with(root))
            {
                mark_missing.execute([o.id])?;
            }
        }
        drop(mark_missing);
        drop(current);
        tx.commit()?;
        Ok(())
    }
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        valid_name(name)?;
        self.conn
            .execute("INSERT INTO playlists(name)VALUES(?)", [name])?;
        Ok(self.conn.last_insert_rowid())
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
        let mut position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?",
            [id],
            |r| r.get(0),
        )?;
        let mut seen = HashSet::with_capacity(tracks.len());
        let mut exists_track_stmt =
            tx.prepare("SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)")?;
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
            if !exists_track_stmt.query_row([track_id], |row| row.get::<_, bool>(0))? {
                bail!("Track {track_id} does not exist");
            }
            position += insert.execute(params![id, track_id, position])? as i64;
        }
        drop(insert);
        drop(exists_track_stmt);
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
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key=?", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE settings.value<>excluded.value",params![key,value])?;
        Ok(())
    }
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
fn changed(n: usize, k: &str, id: i64) -> Result<()> {
    if n == 0 {
        bail!("{k} {id} does not exist")
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
mod tests {
    fn all_tracks(store: &Store) -> Vec<Track> {
        let mut tracks = Vec::new();
        loop {
            let page = store
                .library_page(None, None, None, tracks.len(), crate::model::PAGE_SIZE)
                .unwrap();
            tracks.extend(page.rows);
            if tracks.len() >= page.total {
                return tracks;
            }
        }
    }
    fn playlist_entries(store: &Store, playlist_id: i64) -> Vec<PlaylistEntryRow> {
        store
            .playlist_entries_page(playlist_id, 0, crate::model::PAGE_SIZE)
            .unwrap()
            .rows
    }
    use super::*;
    use crate::library::{ScanRecord, ScanResult};
    fn rec(path: &str, hash: &str, title: &str, dur: f64) -> ScanRecord {
        ScanRecord {
            path: path.into(),
            cue: None,
            size: 1,
            modified_ns: 1,
            fingerprint: Some(hash.into()),
            media: MediaInfo {
                title: title.into(),
                artist: String::new(),
                album: String::new(),
                duration: Some(dur),
                codec: "test".into(),
                channels: 2,
                sample_rate: 48000,
                bitrate_bps: None,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            },
        }
    }
    fn scan(records: Vec<ScanRecord>) -> ScanResult {
        ScanResult {
            roots: vec![PathBuf::from("/music")],
            records,
            errors: vec![],
            suppressed_sources: Vec::new(),
        }
    }
    fn db(name: &str) -> (PathBuf, Store) {
        let p = std::env::temp_dir().join(format!("rivu-{name}-{}.db", std::process::id()));
        let _ = fs::remove_file(&p);
        (p.clone(), Store::open(&p).unwrap())
    }

    #[test]
    fn track_cache_refreshes_hits_before_lru_eviction() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![
                rec("/music/a.mp3", "a", "A", 10.),
                rec("/music/b.mp3", "b", "B", 10.),
                rec("/music/c.mp3", "c", "C", 10.),
            ]))
            .unwrap();
        let ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
        store.set_track_cache_page_size(1); // capacity is two tracks
        store.track(ids[0]).unwrap();
        store.track(ids[1]).unwrap();
        store.track(ids[0]).unwrap(); // refresh A's LRU age
        store
            .conn
            .execute("UPDATE tracks SET title_override='A2' WHERE id=?", [ids[0]])
            .unwrap();
        store
            .conn
            .execute("UPDATE tracks SET title_override='B2' WHERE id=?", [ids[1]])
            .unwrap();
        store.track(ids[2]).unwrap(); // B is oldest and must be evicted

        let cache = store.track_cache.borrow();
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.entries.contains_key(&ids[0]));
        assert!(cache.entries.contains_key(&ids[2]));
        assert!(!cache.entries.contains_key(&ids[1]));
        drop(cache);
        assert_eq!(store.track(ids[0]).unwrap().unwrap().title, "A");
        assert_eq!(store.track(ids[1]).unwrap().unwrap().title, "B2");
        assert!(store.track_cache.borrow().entries.len() <= 2);
    }

    #[test]
    fn track_cache_resize_shrinks_and_does_not_cache_misses() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![
                rec("/music/a.mp3", "a", "A", 10.),
                rec("/music/b.mp3", "b", "B", 10.),
                rec("/music/c.mp3", "c", "C", 10.),
                rec("/music/d.mp3", "d", "D", 10.),
            ]))
            .unwrap();
        let ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
        store.set_track_cache_page_size(2); // capacity is four tracks
        for id in &ids {
            store.track(*id).unwrap();
        }
        store.track(ids[0]).unwrap(); // retain A when shrinking
        store.set_track_cache_page_size(1); // capacity is two tracks
        let cache = store.track_cache.borrow();
        assert_eq!(cache.capacity, 2);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.entries.contains_key(&ids[0]));
        assert!(cache.entries.contains_key(&ids[3]));
        assert!(cache.entries.len() <= cache.capacity);
        drop(cache);

        store.set_track_cache_page_size(0);
        assert!(store.track_cache.borrow().entries.is_empty());
        assert!(store.track(999).unwrap().is_none());
        assert!(store.track_cache.borrow().entries.is_empty());
    }

    #[test]
    fn track_cache_invalidates_edits_favorites_statistics_and_deletes() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 10.)]))
            .unwrap();
        let id = all_tracks(&store)[0].id;
        assert_eq!(store.track(id).unwrap().unwrap().title, "A");

        store.set_favorite(&[id], true).unwrap();
        assert!(store.track(id).unwrap().unwrap().favorite);
        store.edit_track(id, "Edited", "Artist", "Album").unwrap();
        assert_eq!(store.track(id).unwrap().unwrap().title, "Edited");
        store.mark_played(id, 42).unwrap();
        assert_eq!(store.track(id).unwrap().unwrap().last_played, Some(42));
        store.increment_play_count(id).unwrap();
        assert_eq!(store.track(id).unwrap().unwrap().play_count, 1);

        store.remove_tracks(&[id]).unwrap();
        assert!(store.track(id).unwrap().is_none());
        assert!(store.track_cache.borrow().entries.is_empty());
    }

    #[test]
    fn track_cache_rescan_invalidation_returns_new_metadata() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 10.)]))
            .unwrap();
        let id = all_tracks(&store)[0].id;
        store.track(id).unwrap();
        store
            .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "Rescanned", 20.)]))
            .unwrap();
        let track = store.track(id).unwrap().unwrap();
        assert_eq!(track.title, "Rescanned");
        assert_eq!(track.duration, Some(20.));
    }

    #[test]
    fn failed_track_batch_rollback_exposes_no_uncommitted_values() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 10.)]))
            .unwrap();
        let id = all_tracks(&store)[0].id;
        assert!(!store.track(id).unwrap().unwrap().favorite);

        assert!(store.set_favorite(&[id, 999], true).is_err());
        let after_favorite = store.track(id).unwrap().unwrap();
        assert!(!after_favorite.favorite);

        assert!(store.remove_tracks(&[id, 999]).is_err());
        let after_delete = store.track(id).unwrap().unwrap();
        assert_eq!(after_delete.id, id);
        assert!(!after_delete.favorite);
        assert_eq!(after_delete.title, "A");
    }
    #[test]
    fn playlist_deduplicates_additions_and_preserves_first_order() {
        let (p, s) = db("playlist");
        s.apply_scan(&scan(vec![
            rec("/music/a.mp3", "a", "A", 100.),
            rec("/music/b.mp3", "b", "B", 100.),
            rec("/music/c.mp3", "c", "C", 100.),
        ]))
        .unwrap();
        let ids: Vec<_> = all_tracks(&s).iter().map(|track| track.id).collect();
        let pl = s.create_playlist("mix").unwrap();
        s.add_playlist(pl, &[ids[0], ids[1], ids[0]]).unwrap();
        let first_entry = playlist_entries(&s, pl)[0].id;
        s.add_playlist(pl, &[ids[1], ids[2], ids[2]]).unwrap();
        let entries = playlist_entries(&s, pl);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(entries[0].id, first_entry);
        s.move_playlist_entry(entries[2].id, 0).unwrap();
        s.remove_playlist_entry(first_entry).unwrap();
        drop(s);
        assert_eq!(
            playlist_entries(&Store::open(&p).unwrap(), pl)
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            vec![ids[2], ids[1]]
        );
        let _ = fs::remove_file(p);
    }
    #[test]
    fn recent_tracks_merge_replays_without_losing_counts() {
        let (p, s) = db("recent");
        s.apply_scan(&scan(vec![
            rec("/music/a.mp3", "a", "A", 600.),
            rec("/music/b.mp3", "b", "B", 100.),
        ]))
        .unwrap();
        let tracks = all_tracks(&s);
        let (a, b) = (tracks[0].id, tracks[1].id);
        s.mark_played(a, 10).unwrap();
        s.increment_play_count(a).unwrap();
        s.mark_played(b, 20).unwrap();
        s.mark_played(a, 30).unwrap();
        s.increment_play_count(a).unwrap();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(all_tracks(&s)[0].play_count, 2);
        assert_eq!(all_tracks(&s)[1].play_count, 0);
        assert_eq!(
            s.history(10)
                .unwrap()
                .iter()
                .map(|entry| (entry.track_id, entry.played_at))
                .collect::<Vec<_>>(),
            vec![(a, 30), (b, 20)]
        );
        s.remove_tracks(&[a]).unwrap();
        assert_eq!(
            s.history(10)
                .unwrap()
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            vec![b]
        );
        let _ = fs::remove_file(p);
    }
    #[test]
    fn moved_id_preserves_override_and_delete_does_not_reuse() {
        let (p, s) = db("identity");
        s.apply_scan(&scan(vec![rec("/music/a.mp3", "same", "Raw", 100.)]))
            .unwrap();
        let id = all_tracks(&s)[0].id;
        s.edit_track(id, "Override", "A", "B").unwrap();
        s.apply_scan(&scan(vec![rec("/music/b.mp3", "same", "Changed", 100.)]))
            .unwrap();
        assert_eq!(all_tracks(&s)[0].id, id);
        assert_eq!(all_tracks(&s)[0].title, "Override");
        s.remove_tracks(&[id]).unwrap();
        s.apply_scan(&scan(vec![rec("/music/c.mp3", "new", "New", 100.)]))
            .unwrap();
        assert_ne!(all_tracks(&s)[0].id, id);
        let _ = fs::remove_file(p);
    }
    #[test]
    fn deleting_tracks_keeps_playlist_reordering_contiguous() {
        let (path, store) = db("compact");
        store
            .apply_scan(&scan(vec![
                rec("/music/a.wav", "a", "A", 100.0),
                rec("/music/b.wav", "b", "B", 100.0),
                rec("/music/c.wav", "c", "C", 100.0),
            ]))
            .unwrap();
        let tracks = all_tracks(&store);
        let playlist = store.create_playlist("mix").unwrap();
        store
            .add_playlist(
                playlist,
                &[
                    tracks[0].id,
                    tracks[1].id,
                    tracks[0].id,
                    tracks[2].id,
                    tracks[1].id,
                ],
            )
            .unwrap();
        let before = playlist_entries(&store, playlist);
        store.remove_tracks(&[tracks[0].id]).unwrap();
        store.move_playlist_entry(before[2].id, 0).unwrap();
        assert_eq!(
            playlist_entries(&store, playlist)
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![before[2].id, before[1].id]
        );
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn metadata_refresh_preserves_favorites_and_unknown_values() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut record = rec("/music/tagged.wav", "hash", "A", 10.);
        record.media.bitrate_bps = Some(1_536_000);
        record.media.bits_per_sample = Some(16);
        record.media.track_number = Some(2);
        record.media.disc_number = Some(1);
        record.media.release_date = Some("2024-03".into());
        store.apply_scan(&scan(vec![record.clone()])).unwrap();
        let id = all_tracks(&store)[0].id;
        store.set_favorite(&[id, id], true).unwrap();
        store.mark_played(id, 42).unwrap();
        store.increment_play_count(id).unwrap();
        record.media.track_number = Some(3);
        record.media.bitrate_bps = None;
        store.apply_scan(&scan(vec![record])).unwrap();
        let track = all_tracks(&store).remove(0);
        assert!(track.favorite);
        assert_eq!(
            (track.track_number, track.disc_number, track.bits_per_sample),
            (Some(3), Some(1), Some(16))
        );
        assert_eq!(track.release_date.as_deref(), Some("2024-03"));
        assert_eq!(track.bitrate_bps, None);
        assert_eq!((track.play_count, track.last_played), (1, Some(42)));
        assert!(store.set_favorite(&[id, 999], false).is_err());
        assert!(all_tracks(&store)[0].favorite);
        assert_eq!(
            store.known_files().unwrap()[0]
                .media
                .as_ref()
                .unwrap()
                .track_number,
            Some(3)
        );
    }

    #[test]
    fn maintenance_retries_after_reader_releases_and_keeps_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let store = Store::open(&path).unwrap();
        store
            .apply_scan(&scan(vec![rec("/music/a.wav", "hash", "A", 10.)]))
            .unwrap();
        let id = all_tracks(&store)[0].id;
        let reader = Connection::open(&path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT * FROM tracks;")
            .unwrap();
        store.mark_played(id, 42).unwrap();
        store.conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        assert!(store.optimize().is_err());
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT value FROM schema_meta WHERE key='compact_pending'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(all_tracks(&reopened)[0].last_played, Some(42));
        assert_eq!(
            reopened
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM schema_meta WHERE key='compact_pending'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn playlist_addition_with_missing_track_rolls_back_new_entries() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![
                rec("/music/a.wav", "a", "A", 10.),
                rec("/music/b.wav", "b", "B", 10.),
            ]))
            .unwrap();
        let tracks = all_tracks(&store);
        let playlist = store.create_playlist("mix").unwrap();
        store.add_playlist(playlist, &[tracks[0].id]).unwrap();
        assert!(
            store
                .add_playlist(playlist, &[tracks[0].id, tracks[1].id, 999])
                .is_err()
        );
        assert_eq!(
            playlist_entries(&store, playlist)
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            vec![tracks[0].id]
        );
    }

    fn segment(sheet: &str, number: u32, start: u64, end: Option<u64>) -> ScanRecord {
        let mut record = rec(
            "/music/shared.wav",
            "shared",
            &format!("Track {number}"),
            60.0,
        );
        record.cue = Some(CueSegment {
            sheet: sheet.into(),
            number,
            start_frame: start,
            end_frame: end,
        });
        record
    }

    #[test]
    fn shared_source_segments_keep_independent_ids_overrides_stats_and_missing_state() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let whole = rec("/music/shared.wav", "shared", "Whole", 180.0);
        let first = segment("/music/a.cue", 1, 0, Some(4500));
        let second = segment("/music/a.cue", 2, 4500, Some(9000));
        let other = segment("/music/b.cue", 1, 0, Some(4500));
        store
            .apply_scan(&scan(vec![whole, first.clone(), second.clone(), other]))
            .unwrap();
        let tracks = all_tracks(&store);
        let ids: Vec<_> = tracks.iter().map(|t| t.id).collect();
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 4);
        store
            .edit_track(ids[1], "Custom", "Artist", "Album")
            .unwrap();
        store.mark_played(ids[1], 10).unwrap();
        store.increment_play_count(ids[1]).unwrap();
        let playlist = store.create_playlist("Segments").unwrap();
        store
            .add_playlist(playlist, &[ids[1], ids[2], ids[1], ids[3]])
            .unwrap();
        let mut changed = first;
        changed.path = "/music/replaced.wav".into();
        changed.media.title = "New raw title".into();
        changed.media.duration = Some(59.0);
        changed.cue.as_mut().unwrap().start_frame = 75;
        changed.cue.as_mut().unwrap().end_frame = Some(4500);
        let mut rescan = scan(vec![changed.clone(), second.clone()]);
        rescan.roots = vec!["/music/a.cue".into()];
        store.apply_scan(&rescan).unwrap();
        let after = all_tracks(&store);
        assert_eq!(after.iter().map(|t| t.id).collect::<Vec<_>>(), ids);
        assert_eq!(after[1].title, "Custom");
        assert_eq!(after[1].cue, changed.cue);
        assert_eq!(after[1].path, changed.path);
        assert_eq!(after[1].duration, Some(59.0));
        assert_eq!((after[1].play_count, after[1].last_played), (1, Some(10)));
        for index in [0, 2, 3] {
            assert_eq!(
                (after[index].play_count, after[index].last_played),
                (0, None)
            );
            assert!(!after[index].missing);
        }
        let known = store.known_files().unwrap();
        let known = known.iter().find(|r| r.track_id == ids[1]).unwrap();
        assert_eq!(known.cue, changed.cue);
        assert_eq!(known.media.as_ref().unwrap().title, "New raw title");
        rescan.records = vec![second];
        store.apply_scan(&rescan).unwrap();
        let after = all_tracks(&store);
        assert!(after[1].missing);
        for index in [0, 2, 3] {
            assert!(!after[index].missing);
        }
        assert_eq!(playlist_entries(&store, playlist).len(), 3);
        rescan.records.push(changed);
        store.apply_scan(&rescan).unwrap();
        assert!(!all_tracks(&store)[1].missing);
        store.remove_tracks(&[ids[1]]).unwrap();
        assert_eq!(
            all_tracks(&store).iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![ids[0], ids[2], ids[3]]
        );
        assert_eq!(
            playlist_entries(&store, playlist)
                .iter()
                .map(|e| e.track_id)
                .collect::<Vec<_>>(),
            vec![ids[2], ids[3]]
        );
        assert!(store.history(10).unwrap().is_empty());
    }

    #[test]
    fn directory_suppression_preserves_whole_source_but_missing_sheets_mark_segments() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let first = segment("/music/a.cue", 1, 0, Some(4500));
        store
            .apply_scan(&scan(vec![
                rec("/music/shared.wav", "shared", "Whole", 180.0),
                first.clone(),
            ]))
            .unwrap();
        let mut result = scan(vec![first]);
        result.suppressed_sources.push("/music/shared.wav".into());
        store.apply_scan(&result).unwrap();
        assert!(all_tracks(&store).iter().all(|t| !t.missing));
        result.records.clear();
        store.apply_scan(&result).unwrap();
        let tracks = all_tracks(&store);
        assert!(!tracks[0].missing);
        assert!(tracks[1].missing);
    }

    #[test]
    fn fingerprints_never_merge_full_files_or_different_cue_segments() {
        let root = std::env::temp_dir().join(format!("rivu-cue-matches-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let cases = [
            (None, Some((1, 0, Some(4500))), false),
            (Some((1, 0, Some(4500))), None, false),
            (Some((1, 0, Some(4500))), Some((2, 0, Some(4500))), false),
            (Some((1, 0, Some(4500))), Some((1, 75, Some(4500))), false),
            (Some((1, 0, Some(4500))), Some((1, 0, None)), false),
            (Some((1, 0, Some(4500))), Some((1, 0, Some(4500))), true),
        ];
        for (index, (before, after, same_id)) in cases.into_iter().enumerate() {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let make_cue = |value: Option<(u32, u64, Option<u64>)>, name: &str| {
                value.map(|(number, start_frame, end_frame)| CueSegment {
                    sheet: root.join(format!("{index}-{name}.cue")),
                    number,
                    start_frame,
                    end_frame,
                })
            };
            let mut old = rec("", "same-hash", "Old", 60.0);
            old.path = root.join(format!("{index}-old.wav"));
            old.cue = make_cue(before, "old");
            let mut new = rec("", "same-hash", "New", 60.0);
            new.path = root.join(format!("{index}-new.wav"));
            new.cue = make_cue(after, "new");
            let mut result = scan(vec![old]);
            result.roots = vec![root.clone()];
            store.apply_scan(&result).unwrap();
            let old_id = all_tracks(&store)[0].id;
            result.records = vec![new];
            store.apply_scan(&result).unwrap();
            let tracks = all_tracks(&store);
            let new_track = tracks.iter().find(|t| t.title == "New").unwrap();
            assert_eq!(new_track.id == old_id, same_id, "case {index}");
            assert_eq!(tracks.len(), if same_id { 1 } else { 2 });
            assert!(!new_track.missing);
            if !same_id {
                assert!(tracks.iter().find(|t| t.id == old_id).unwrap().missing);
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cue_relocation_requires_absent_sheet_not_absent_shared_source() {
        let root = std::env::temp_dir().join(format!("rivu-cue-move-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let sheet = root.join("old.cue");
        let source = root.join("shared.wav");
        fs::write(&sheet, b"sheet").unwrap();
        fs::write(&source, b"source").unwrap();
        let mut old = segment(sheet.to_str().unwrap(), 1, 0, Some(4500));
        old.path = source.clone();
        let mut moved = old.clone();
        moved.cue.as_mut().unwrap().sheet = root.join("new.cue");
        for sheet_exists in [true, false] {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let mut result = scan(vec![old.clone()]);
            result.roots = vec![root.clone()];
            store.apply_scan(&result).unwrap();
            let id = all_tracks(&store)[0].id;
            store.edit_track(id, "Override", "Artist", "Album").unwrap();
            if !sheet_exists {
                fs::remove_file(&sheet).unwrap();
            }
            result.records = vec![moved.clone()];
            store.apply_scan(&result).unwrap();
            let tracks = all_tracks(&store);
            let new = tracks.iter().find(|t| t.cue == moved.cue).unwrap();
            assert_eq!(new.id == id, !sheet_exists);
            if !sheet_exists {
                assert_eq!(new.title, "Override");
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ambiguous_cue_relocation_does_not_steal_an_existing_id() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![
                segment("/music/old-a.cue", 1, 0, Some(4500)),
                segment("/music/old-b.cue", 1, 0, Some(4500)),
            ]))
            .unwrap();
        let ids: Vec<_> = all_tracks(&store).iter().map(|t| t.id).collect();
        store
            .apply_scan(&scan(vec![segment("/music/new.cue", 1, 0, Some(4500))]))
            .unwrap();
        let tracks = all_tracks(&store);
        assert_eq!(tracks.len(), 3);
        assert!(!ids.contains(&tracks[2].id));
    }

    #[test]
    fn invalid_cue_database_range_leaves_entire_scan_unchanged() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![rec(
                "/music/original.wav",
                "old",
                "Original",
                60.0,
            )]))
            .unwrap();
        let invalid = segment("/music/a.cue", 1, u64::MAX, None);
        assert!(
            store
                .apply_scan(&scan(vec![
                    rec("/music/new.wav", "new", "New", 60.0),
                    invalid
                ]))
                .is_err()
        );
        let tracks = all_tracks(&store);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Original");
        assert!(!tracks[0].missing);
    }
    #[test]
    fn bounded_view_queries_filter_order_and_count() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![
                rec("/music/c.mp3", "c", "Charlie", 1.0),
                rec("/music/a.mp3", "a", "Alpha", 1.0),
                rec("/music/b.mp3", "b", "Bravo", 1.0),
            ]))
            .unwrap();
        let ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
        store.set_favorite(&[ids[1]], true).unwrap();
        let page = store.library_page(None, Some(true), None, 0, 1).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].id, ids[1]);
        let page = store.library_page(Some("a"), None, None, 0, 2).unwrap();
        assert_eq!(
            page.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
            vec![ids[0], ids[1]]
        );

        let playlist = store.create_playlist("mix").unwrap();
        store.add_playlist(playlist, &ids).unwrap();
        assert_eq!(store.playlist_summaries(0, 1).unwrap()[0].entry_count, 3);
        let entries = store.playlist_entries_page(playlist, 1, 1).unwrap();
        assert_eq!(entries.total, 3);
        assert_eq!(entries.rows.len(), 1);
        assert_eq!(entries.rows[0].track_id, ids[1]);
        assert_eq!(store.playlist_track_ids(playlist, 2).unwrap(), ids[..2]);
    }

    #[test]
    fn directory_pages_preserve_boundaries_and_duplicate_cue_sources() {
        use crate::model::{DirectoryRow, PAGE_SIZE};
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut records = (0..300)
            .map(|index| {
                rec(
                    &format!("/music/Album/{index:03}.wav"),
                    &format!("hash-{index}"),
                    &format!("Track {index}"),
                    60.0,
                )
            })
            .collect::<Vec<_>>();
        records.push(rec("/music/AlbumExtra/other.wav", "other", "Other", 60.0));
        records.push(segment("/music/Album/sheet.cue", 1, 0, Some(750)));
        records.push(segment("/music/Album/sheet.cue", 2, 750, None));
        store.apply_scan(&scan(records)).unwrap();
        let root = store.directory_page(Path::new(""), 0, PAGE_SIZE).unwrap();
        assert_eq!(root.rows, [DirectoryRow::Directory { path: "/".into() }]);
        let music = store
            .directory_page(Path::new("/music"), 0, PAGE_SIZE)
            .unwrap();
        assert_eq!(music.total, 4);
        assert!(
            matches!(&music.rows[0], DirectoryRow::Directory { path } if path == Path::new("/music/Album"))
        );
        let first = store
            .directory_page(Path::new("/music/Album/"), 0, usize::MAX)
            .unwrap();
        assert_eq!(first.total, 300);
        assert_eq!(first.rows.len(), PAGE_SIZE);
        let last = store
            .directory_page(Path::new("/music/Album"), PAGE_SIZE, PAGE_SIZE)
            .unwrap();
        assert_eq!(last.rows.len(), 44);
        assert!(matches!(&last.rows[43], DirectoryRow::Track(row) if row.title == "Track 299"));
        let cue_sources = store
            .directory_page(Path::new("/music"), 2, PAGE_SIZE)
            .unwrap();
        assert_eq!(cue_sources.rows.len(), 2);
        assert!(
            cue_sources
                .rows
                .iter()
                .all(|row| matches!(row, DirectoryRow::Track(_)))
        );
        let tail_id = store
            .track_id_for_source(Path::new("/music/Album/299.wav"), None)
            .unwrap()
            .unwrap();
        assert_eq!(store.track(tail_id).unwrap().unwrap().title, "Track 299");
        let cue_id = store
            .track_id_for_source(Path::new("/music/Album/sheet.cue"), Some(2))
            .unwrap()
            .unwrap();
        assert_eq!(store.track(cue_id).unwrap().unwrap().cue.unwrap().number, 2);
        assert!(
            store
                .track_id_for_source(Path::new("/music/shared.wav"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn library_pages_use_literal_unicode_search_and_database_sorting() {
        use crate::model::LibrarySort;
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&scan(vec![
                rec("/music/a.wav", "a", "ÉTÉ_100%", 60.0),
                rec("/music/b.wav", "b", "Other", 60.0),
            ]))
            .unwrap();
        let literal = store
            .library_view_page(Some("été_100%"), None, None, LibrarySort::Id, 0, 256)
            .unwrap();
        assert_eq!(literal.total, 1);
        assert_eq!(literal.rows[0].title, "ÉTÉ_100%");
        assert_eq!(
            store
                .library_page(Some("été_100%"), None, None, 0, 256)
                .unwrap()
                .total,
            1
        );
        let id = store
            .library_page(Some("Other"), None, None, 0, 1)
            .unwrap()
            .rows[0]
            .id;
        store.increment_play_count(id).unwrap();
        let ranked = store
            .library_view_page(None, None, None, LibrarySort::MostPlayed, 0, 1)
            .unwrap();
        assert_eq!(ranked.rows[0].id, id);
        assert_eq!(store.library_stats().unwrap().play_count, 1);
        assert_eq!(
            store
                .library_view_page(None, None, None, LibrarySort::Id, 2, 256)
                .unwrap()
                .rows,
            []
        );
    }

    #[test]
    fn directory_selections_deduplicate_atomically_without_capping_playlists() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<5000) INSERT INTO tracks(path) SELECT '/music/' || CASE WHEN n<=2500 THEN 'A/' ELSE 'B/' END || n || '.wav' FROM ids").unwrap();
        let directories = vec![
            PathBuf::from("/music/A"),
            PathBuf::from("/music/B"),
            PathBuf::from("/music/A"),
        ];
        assert!(store.source_track_ids(&directories, &[1], 4096).is_err());
        let ids = store
            .source_track_ids(&directories[..1], &[1, 1], 2500)
            .unwrap();
        assert_eq!(ids.len(), 2500);
        assert_eq!((ids[0], ids[2499]), (1, 2500));
        let playlist = store.create_playlist("Unlimited").unwrap();
        store.add_playlist(playlist, &[5000]).unwrap();
        store
            .add_playlist_sources(playlist, &directories, &[1, 1])
            .unwrap();
        let first = store.playlist_entries_page(playlist, 0, 2).unwrap();
        assert_eq!(first.total, 5000);
        assert_eq!(
            first
                .rows
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [5000, 1]
        );
        let last = store.playlist_entries_page(playlist, 4999, 1).unwrap();
        assert_eq!(last.rows[0].track_id, 4999);
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT MAX(position) FROM playlist_entries WHERE playlist_id=?1",
                    [playlist],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            4999
        );
        store
            .add_playlist_sources(playlist, &directories, &[])
            .unwrap();
        assert_eq!(
            store.playlist_entries_page(playlist, 0, 0).unwrap().total,
            5000
        );
        let empty = store.create_playlist("Atomic").unwrap();
        assert!(
            store
                .add_playlist_sources(empty, &directories, &[99999])
                .is_err()
        );
        assert_eq!(store.playlist_entries_page(empty, 0, 0).unwrap().total, 0);
    }

    #[test]
    fn bulk_missing_cleanup_spans_pages_and_preserves_playlist_reordering() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store.conn.execute_batch("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<1000) INSERT INTO tracks(path,missing) SELECT '/music/'||n||'.wav',n<=500 FROM ids").unwrap();
        let playlist = store.create_playlist("Availability").unwrap();
        store.add_playlist(playlist, &[1000]).unwrap();
        store
            .add_playlist_sources(playlist, &[PathBuf::from("/music")], &[])
            .unwrap();
        store.mark_played(25, 42).unwrap();
        store.mark_played(1000, 43).unwrap();
        store.remove_missing_tracks().unwrap();
        assert_eq!(store.library_stats().unwrap().total, 500);
        assert_eq!(
            store
                .library_page(None, None, Some(true), 0, 1)
                .unwrap()
                .total,
            0
        );
        assert_eq!(
            store
                .history(200)
                .unwrap()
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [1000]
        );
        let page = store.playlist_entries_page(playlist, 0, 1).unwrap();
        assert_eq!(page.total, 500);
        assert_eq!(page.rows[0].track_id, 1000);
        store.move_playlist_entry(page.rows[0].id, 499).unwrap();
        assert_eq!(
            store.playlist_entries_page(playlist, 499, 1).unwrap().rows[0].track_id,
            1000
        );
    }
}
