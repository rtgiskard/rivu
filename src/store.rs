use crate::library::{KnownFile, MediaInfo, ScanResult};
use crate::model::{
    CueSegment, DatabaseOptimization, HistoryEntry, Playlist, PlaylistEntry, Track,
};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// Bump when probing semantics change; fingerprints remain reusable.
const PROBE_VERSION: i64 = 3;

pub struct Store {
    conn: Connection,
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
        let store = Self { conn: c };
        let pending: bool = store.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_meta WHERE key='compact_pending' AND value=1)",
            [],
            |row| row.get(0),
        )?;
        if pending && let Err(error) = store.optimize() {
            eprintln!("rivu: database maintenance pending (will retry): {error:#}");
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
    pub fn set_favorite(&self, ids: &[i64], favorite: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for &id in ids {
            exists_track(&tx, id)?;
            tx.execute(
                "UPDATE tracks SET favorite=?1 WHERE id=?2 AND favorite<>?1",
                params![favorite, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn tracks(&self) -> Result<Vec<Track>> {
        let mut q=self.conn.prepare("SELECT id,path,COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),COALESCE(album_override,raw_album),duration,codec,channels,sample_rate,missing,play_count,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame,bitrate_bps,track_number,disc_number,bits_per_sample,release_date,favorite,fingerprint FROM tracks ORDER BY id")?;
        Ok(q.query_map([], |r| {
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
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut q = self
            .conn
            .prepare("SELECT id,name FROM playlists ORDER BY id")?;
        let rows = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name) = row?;
            let mut e = self.conn.prepare(
                "SELECT id,track_id FROM playlist_entries WHERE playlist_id=? ORDER BY position,id",
            )?;
            out.push(Playlist {
                id,
                name,
                entries: e
                    .query_map([id], |r| {
                        Ok(PlaylistEntry {
                            id: r.get(0)?,
                            track_id: r.get(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            });
        }
        Ok(out)
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
            let id = if let Some(id) = id {
                tx.execute("UPDATE tracks SET path=?,fingerprint=?,file_size=?,modified_ns=?,raw_title=?,raw_artist=?,raw_album=?,duration=?,codec=?,channels=?,sample_rate=?,cue_sheet=?,cue_number=?,cue_start_frame=?,cue_end_frame=?,missing=0,probe_version=?,bitrate_bps=?,track_number=?,disc_number=?,bits_per_sample=?,release_date=? WHERE id=?",params![r.path.to_str(),r.fingerprint.as_deref(),r.size as i64,r.modified_ns,m.title,m.artist,m.album,m.duration,m.codec,m.channels as i64,m.sample_rate as i64,sheet,number,start,end,PROBE_VERSION,bitrate,m.track_number,m.disc_number,m.bits_per_sample,m.release_date,id])?;
                id
            } else {
                tx.execute("INSERT INTO tracks(path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,bits_per_sample,release_date) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![r.path.to_str(),r.fingerprint.as_deref(),r.size as i64,r.modified_ns,m.title,m.artist,m.album,m.duration,m.codec,m.channels as i64,m.sample_rate as i64,sheet,number,start,end,PROBE_VERSION,bitrate,m.track_number,m.disc_number,m.bits_per_sample,m.release_date])?;
                tx.last_insert_rowid()
            };
            byidentity.insert(key, id);
            seen.insert(id);
        }
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
                tx.execute("UPDATE tracks SET missing=1 WHERE id=?", [o.id])?;
            }
        }
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
        for &track_id in tracks {
            if !seen.insert(track_id) {
                continue;
            }
            exists_track(&tx, track_id)?;
            position += tx.execute(
                "INSERT INTO playlist_entries(playlist_id,track_id,position)
                 SELECT ?1,?2,?3 WHERE NOT EXISTS(
                     SELECT 1 FROM playlist_entries WHERE playlist_id=?1 AND track_id=?2
                 )",
                params![id, track_id, position],
            )? as i64;
        }
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
        let tx = self.conn.unchecked_transaction()?;
        let mut affected = HashSet::new();
        for id in ids {
            exists_track(&tx, *id)?;
            let mut query =
                tx.prepare("SELECT DISTINCT playlist_id FROM playlist_entries WHERE track_id=?")?;
            affected.extend(
                query
                    .query_map([id], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
            tx.execute("DELETE FROM playlist_entries WHERE track_id=?", [id])?;
            tx.execute("DELETE FROM tracks WHERE id=?", [id])?;
        }
        for playlist in affected {
            tx.execute("WITH ordered AS (SELECT id, ROW_NUMBER() OVER (ORDER BY position,id)-1 AS new_position FROM playlist_entries WHERE playlist_id=?1) UPDATE playlist_entries SET position=(SELECT new_position FROM ordered WHERE ordered.id=playlist_entries.id) WHERE playlist_id=?1", [playlist])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn mark_played(&self, track: i64, at: i64) -> Result<()> {
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
fn exists_track(tx: &Transaction<'_>, id: i64) -> Result<()> {
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?)",
        [id],
        |r| r.get(0),
    )? {
        bail!("track {id} does not exist");
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
    fn playlist_deduplicates_additions_and_preserves_first_order() {
        let (p, s) = db("playlist");
        s.apply_scan(&scan(vec![
            rec("/music/a.mp3", "a", "A", 100.),
            rec("/music/b.mp3", "b", "B", 100.),
            rec("/music/c.mp3", "c", "C", 100.),
        ]))
        .unwrap();
        let ids: Vec<_> = s.tracks().unwrap().iter().map(|track| track.id).collect();
        let pl = s.create_playlist("mix").unwrap();
        s.add_playlist(pl, &[ids[0], ids[1], ids[0]]).unwrap();
        let first_entry = s.playlists().unwrap()[0].entries[0].id;
        s.add_playlist(pl, &[ids[1], ids[2], ids[2]]).unwrap();
        let entries = s.playlists().unwrap().remove(0).entries;
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
            Store::open(&p).unwrap().playlists().unwrap()[0]
                .entries
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
        let tracks = s.tracks().unwrap();
        let (a, b) = (tracks[0].id, tracks[1].id);
        s.mark_played(a, 10).unwrap();
        s.increment_play_count(a).unwrap();
        s.mark_played(b, 20).unwrap();
        s.mark_played(a, 30).unwrap();
        s.increment_play_count(a).unwrap();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.tracks().unwrap()[0].play_count, 2);
        assert_eq!(s.tracks().unwrap()[1].play_count, 0);
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
        let id = s.tracks().unwrap()[0].id;
        s.edit_track(id, "Override", "A", "B").unwrap();
        s.apply_scan(&scan(vec![rec("/music/b.mp3", "same", "Changed", 100.)]))
            .unwrap();
        assert_eq!(s.tracks().unwrap()[0].id, id);
        assert_eq!(s.tracks().unwrap()[0].title, "Override");
        s.remove_tracks(&[id]).unwrap();
        s.apply_scan(&scan(vec![rec("/music/c.mp3", "new", "New", 100.)]))
            .unwrap();
        assert_ne!(s.tracks().unwrap()[0].id, id);
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
        let tracks = store.tracks().unwrap();
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
        let before = store.playlists().unwrap().remove(0).entries;
        store.remove_tracks(&[tracks[0].id]).unwrap();
        store.move_playlist_entry(before[2].id, 0).unwrap();
        assert_eq!(
            store.playlists().unwrap()[0]
                .entries
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
        let id = store.tracks().unwrap()[0].id;
        store.set_favorite(&[id, id], true).unwrap();
        store.mark_played(id, 42).unwrap();
        store.increment_play_count(id).unwrap();
        record.media.track_number = Some(3);
        record.media.bitrate_bps = None;
        store.apply_scan(&scan(vec![record])).unwrap();
        let track = store.tracks().unwrap().remove(0);
        assert!(track.favorite);
        assert_eq!(
            (track.track_number, track.disc_number, track.bits_per_sample),
            (Some(3), Some(1), Some(16))
        );
        assert_eq!(track.release_date.as_deref(), Some("2024-03"));
        assert_eq!(track.bitrate_bps, None);
        assert_eq!((track.play_count, track.last_played), (1, Some(42)));
        assert!(store.set_favorite(&[id, 999], false).is_err());
        assert!(store.tracks().unwrap()[0].favorite);
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
        let id = store.tracks().unwrap()[0].id;
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
        assert_eq!(reopened.tracks().unwrap()[0].last_played, Some(42));
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
        let tracks = store.tracks().unwrap();
        let playlist = store.create_playlist("mix").unwrap();
        store.add_playlist(playlist, &[tracks[0].id]).unwrap();
        assert!(
            store
                .add_playlist(playlist, &[tracks[0].id, tracks[1].id, 999])
                .is_err()
        );
        assert_eq!(
            store.playlists().unwrap()[0]
                .entries
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
        let tracks = store.tracks().unwrap();
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
        let after = store.tracks().unwrap();
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
        let after = store.tracks().unwrap();
        assert!(after[1].missing);
        for index in [0, 2, 3] {
            assert!(!after[index].missing);
        }
        assert_eq!(store.playlists().unwrap()[0].entries.len(), 3);
        rescan.records.push(changed);
        store.apply_scan(&rescan).unwrap();
        assert!(!store.tracks().unwrap()[1].missing);
        store.remove_tracks(&[ids[1]]).unwrap();
        assert_eq!(
            store
                .tracks()
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![ids[0], ids[2], ids[3]]
        );
        assert_eq!(
            store.playlists().unwrap()[0]
                .entries
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
        assert!(store.tracks().unwrap().iter().all(|t| !t.missing));
        result.records.clear();
        store.apply_scan(&result).unwrap();
        let tracks = store.tracks().unwrap();
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
            let old_id = store.tracks().unwrap()[0].id;
            result.records = vec![new];
            store.apply_scan(&result).unwrap();
            let tracks = store.tracks().unwrap();
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
            let id = store.tracks().unwrap()[0].id;
            store.edit_track(id, "Override", "Artist", "Album").unwrap();
            if !sheet_exists {
                fs::remove_file(&sheet).unwrap();
            }
            result.records = vec![moved.clone()];
            store.apply_scan(&result).unwrap();
            let tracks = store.tracks().unwrap();
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
        let ids: Vec<_> = store.tracks().unwrap().iter().map(|t| t.id).collect();
        store
            .apply_scan(&scan(vec![segment("/music/new.cue", 1, 0, Some(4500))]))
            .unwrap();
        let tracks = store.tracks().unwrap();
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
        let tracks = store.tracks().unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Original");
        assert!(!tracks[0].missing);
    }

}
