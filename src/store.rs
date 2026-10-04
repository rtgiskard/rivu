use crate::library::{KnownFile, MediaInfo, ScanResult};
use crate::model::{CueSegment, HistoryEntry, Playlist, PlaylistEntry, Track};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

fn migrate_v2(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let high_water: Option<i64> = tx
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='tracks'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    // No foreign keys reference tracks: playlist entries and sessions retain their IDs.
    tx.execute_batch(
        "
        CREATE TABLE tracks_v2(
            id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL,fingerprint TEXT,
            file_size INTEGER NOT NULL DEFAULT 0,modified_ns INTEGER NOT NULL DEFAULT 0,
            raw_title TEXT NOT NULL DEFAULT '',raw_artist TEXT NOT NULL DEFAULT '',
            raw_album TEXT NOT NULL DEFAULT '',title_override TEXT,artist_override TEXT,
            album_override TEXT,duration REAL,codec TEXT NOT NULL DEFAULT '',
            channels INTEGER NOT NULL DEFAULT 0,sample_rate INTEGER NOT NULL DEFAULT 0,
            missing INTEGER NOT NULL DEFAULT 0,play_count INTEGER NOT NULL DEFAULT 0,
            listen_seconds REAL NOT NULL DEFAULT 0,last_played INTEGER,
            cue_sheet TEXT,cue_number INTEGER,cue_start_frame INTEGER,cue_end_frame INTEGER
        );
        INSERT INTO tracks_v2(id,path,fingerprint,file_size,modified_ns,raw_title,raw_artist,
            raw_album,title_override,artist_override,album_override,duration,codec,channels,
            sample_rate,missing,play_count,listen_seconds,last_played)
        SELECT id,path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,
            title_override,artist_override,album_override,duration,codec,channels,sample_rate,
            missing,play_count,listen_seconds,last_played FROM tracks;
        DROP TABLE tracks;
        ALTER TABLE tracks_v2 RENAME TO tracks;
        CREATE INDEX tracks_fingerprint ON tracks(fingerprint);
        CREATE UNIQUE INDEX tracks_path ON tracks(path) WHERE cue_sheet IS NULL;
        CREATE UNIQUE INDEX tracks_cue ON tracks(cue_sheet,cue_number) WHERE cue_sheet IS NOT NULL;
    ",
    )?;
    if let Some(high_water) = high_water {
        // MAX(id) is insufficient when the highest issued ID has already been deleted.
        let updated = tx.execute(
            "UPDATE sqlite_sequence SET seq=MAX(seq,?) WHERE name='tracks'",
            [high_water],
        )?;
        if updated == 0 {
            tx.execute(
                "INSERT INTO sqlite_sequence(name,seq) VALUES('tracks',?)",
                [high_water],
            )?;
        }
    }
    tx.execute("UPDATE schema_meta SET value=2 WHERE key='version'", [])?;
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
        c.execute_batch("PRAGMA synchronous=NORMAL; CREATE TABLE IF NOT EXISTS schema_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL); INSERT OR IGNORE INTO schema_meta VALUES('version',0);")?;
        let v: i64 = c.query_row(
            "SELECT value FROM schema_meta WHERE key='version'",
            [],
            |r| r.get(0),
        )?;
        if v > 2 {
            bail!("unsupported schema version {v}");
        }
        if v < 1 {
            c.execute_batch("BEGIN; CREATE TABLE tracks(id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL UNIQUE,fingerprint TEXT,file_size INTEGER NOT NULL DEFAULT 0,modified_ns INTEGER NOT NULL DEFAULT 0,raw_title TEXT NOT NULL DEFAULT '',raw_artist TEXT NOT NULL DEFAULT '',raw_album TEXT NOT NULL DEFAULT '',title_override TEXT,artist_override TEXT,album_override TEXT,duration REAL,codec TEXT NOT NULL DEFAULT '',channels INTEGER NOT NULL DEFAULT 0,sample_rate INTEGER NOT NULL DEFAULT 0,missing INTEGER NOT NULL DEFAULT 0,play_count INTEGER NOT NULL DEFAULT 0,listen_seconds REAL NOT NULL DEFAULT 0,last_played INTEGER); CREATE INDEX tracks_fingerprint ON tracks(fingerprint); CREATE TABLE playlists(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL); CREATE TABLE playlist_entries(id INTEGER PRIMARY KEY AUTOINCREMENT,playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,track_id INTEGER NOT NULL,position INTEGER NOT NULL); CREATE INDEX playlist_order ON playlist_entries(playlist_id,position,id); CREATE TABLE sessions(id INTEGER PRIMARY KEY AUTOINCREMENT,track_id INTEGER NOT NULL,title TEXT NOT NULL DEFAULT '',started_at INTEGER NOT NULL,ended_at INTEGER,listened_seconds REAL NOT NULL DEFAULT 0,counted INTEGER NOT NULL DEFAULT 0,reason TEXT NOT NULL DEFAULT ''); CREATE INDEX session_order ON sessions(started_at DESC,id DESC); UPDATE schema_meta SET value=1 WHERE key='version'; COMMIT;")?;
        }
        if v < 2 {
            migrate_v2(&c)?;
        }
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL)",
        )?;
        let s = Self { conn: c };
        s.finish_interrupted()?;
        Ok(s)
    }
    pub fn tracks(&self) -> Result<Vec<Track>> {
        let mut q=self.conn.prepare("SELECT id,path,COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),COALESCE(album_override,raw_album),duration,codec,channels,sample_rate,missing,play_count,listen_seconds,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame FROM tracks ORDER BY id")?;
        Ok(q.query_map([], |r| {
            Ok(Track {
                id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                title: r.get(2)?,
                artist: r.get(3)?,
                album: r.get(4)?,
                duration: r.get(5)?,
                codec: r.get(6)?,
                channels: r.get::<_, i64>(7)? as u16,
                sample_rate: r.get::<_, i64>(8)? as u32,
                missing: r.get::<_, i64>(9)? != 0,
                play_count: r.get::<_, i64>(10)? as u64,
                listen_seconds: r.get(11)?,
                last_played: r.get(12)?,
                cue: cue_from_row(r, 13)?,
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
        let mut q=self.conn.prepare("SELECT s.id,s.track_id,COALESCE(NULLIF(s.title,''),t.title_override,t.raw_title,''),s.started_at,s.ended_at,s.listened_seconds,s.counted,s.reason FROM sessions s LEFT JOIN tracks t ON t.id=s.track_id ORDER BY s.started_at DESC,s.id DESC LIMIT ?")?;
        Ok(q.query_map([limit.min(i64::MAX as usize) as i64], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                track_id: r.get(1)?,
                title: r.get(2)?,
                started_at: r.get(3)?,
                ended_at: r.get(4)?,
                listened_seconds: r.get(5)?,
                counted: r.get::<_, i64>(6)? != 0,
                reason: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn known_files(&self) -> Result<Vec<KnownFile>> {
        let mut q=self.conn.prepare("SELECT id,path,file_size,modified_ns,fingerprint,raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,cue_end_frame FROM tracks")?;
        Ok(q.query_map([], |r| {
            Ok(KnownFile {
                track_id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                size: r.get::<_, i64>(2)? as u64,
                modified_ns: r.get(3)?,
                fingerprint: r.get(4)?,
                cue: cue_from_row(r, 12)?,
                media: Some(MediaInfo {
                    title: r.get(5)?,
                    artist: r.get(6)?,
                    album: r.get(7)?,
                    duration: r.get(8)?,
                    codec: r.get(9)?,
                    channels: r.get::<_, i64>(10)? as u16,
                    sample_rate: r.get::<_, i64>(11)? as u32,
                }),
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
            let id = if let Some(id) = id {
                tx.execute("UPDATE tracks SET path=?,fingerprint=?,file_size=?,modified_ns=?,raw_title=?,raw_artist=?,raw_album=?,duration=?,codec=?,channels=?,sample_rate=?,cue_sheet=?,cue_number=?,cue_start_frame=?,cue_end_frame=?,missing=0 WHERE id=?",params![r.path.to_str(),r.fingerprint.as_deref(),r.size as i64,r.modified_ns,m.title,m.artist,m.album,m.duration,m.codec,m.channels as i64,m.sample_rate as i64,sheet,number,start,end,id])?;
                id
            } else {
                tx.execute("INSERT INTO tracks(path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,cue_end_frame) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![r.path.to_str(),r.fingerprint.as_deref(),r.size as i64,r.modified_ns,m.title,m.artist,m.album,m.duration,m.codec,m.channels as i64,m.sample_rate as i64,sheet,number,start,end])?;
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
        let first_position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position)+1,0)FROM playlist_entries WHERE playlist_id=?",
            [id],
            |r| r.get(0),
        )?;
        for (index, &track_id) in tracks.iter().enumerate() {
            exists_track(&tx, track_id)?;
            tx.execute(
                "INSERT INTO playlist_entries(playlist_id,track_id,position)VALUES(?,?,?)",
                params![id, track_id, first_position + index as i64],
            )?;
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
    pub fn begin_session(&self, track: i64, start: i64) -> Result<i64> {
        let title: String = self
            .conn
            .query_row(
                "SELECT COALESCE(title_override,raw_title,'')FROM tracks WHERE id=?",
                [track],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("track {track} does not exist"))?;
        self.conn.execute(
            "INSERT INTO sessions(track_id,title,started_at)VALUES(?,?,?)",
            params![track, title, start],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn update_session(
        &self,
        id: i64,
        heard: f64,
        end: Option<i64>,
        reason: &str,
    ) -> Result<()> {
        if !heard.is_finite() || heard < 0.0 {
            bail!("invalid listened seconds");
        }
        let tx = self.conn.unchecked_transaction()?;
        update_tx(&tx, id, heard, end, reason)?;
        tx.commit()?;
        Ok(())
    }
    fn finish_interrupted(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let ids: Vec<i64> = {
            let mut q = tx.prepare("SELECT id FROM sessions WHERE ended_at IS NULL")?;
            q.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in ids {
            update_tx(&tx, id, -1.0, Some(now()), "interrupted")?;
        }
        tx.commit()?;
        Ok(())
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
        self.conn.execute("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
        Ok(())
    }
}
fn update_tx(
    tx: &Transaction<'_>,
    id: i64,
    heard: f64,
    end: Option<i64>,
    reason: &str,
) -> Result<()> {
    let(track,old,counted,dur,oldend):(i64,f64,i64,Option<f64>,Option<i64>)=tx.query_row("SELECT s.track_id,s.listened_seconds,s.counted,t.duration,s.ended_at FROM sessions s LEFT JOIN tracks t ON t.id=s.track_id WHERE s.id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?.ok_or_else(||anyhow!("session {id} does not exist"))?;
    if oldend.is_some() {
        return Ok(());
    }
    let new = if heard < 0.0 { old } else { heard.max(old) };
    let delta = new - old;
    let threshold = dur
        .map(|duration| (duration / 2.0).clamp(0.0, 240.0))
        .unwrap_or(240.0);
    let count = counted == 0 && new >= threshold;
    tx.execute("UPDATE sessions SET listened_seconds=?,ended_at=?,reason=CASE WHEN ?='' THEN reason ELSE ? END,counted=CASE WHEN ? THEN 1 ELSE counted END WHERE id=?",params![new,end,reason,reason,count,id])?;
    if delta > 0.0 || count {
        tx.execute("UPDATE tracks SET listen_seconds=listen_seconds+?,play_count=play_count+?,last_played=CASE WHEN ?>0 THEN ? ELSE last_played END WHERE id=?",params![delta,count as i64,count as i64,end.unwrap_or_else(now),track])?;
    }
    Ok(())
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
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    fn duplicate_playlist_order_survives() {
        let (p, s) = db("playlist");
        s.apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 100.)]))
            .unwrap();
        let t = s.tracks().unwrap()[0].id;
        let pl = s.create_playlist("mix").unwrap();
        s.add_playlist(pl, &[t, t]).unwrap();
        let e = s.playlists().unwrap().remove(0).entries;
        s.move_playlist_entry(e[1].id, 0).unwrap();
        let x = s.playlists().unwrap().remove(0).entries;
        assert_eq!(
            x.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![e[1].id, e[0].id]
        );
        s.remove_playlist_entry(x[0].id).unwrap();
        drop(s);
        assert_eq!(
            Store::open(&p).unwrap().playlists().unwrap()[0].entries[0].id,
            e[0].id
        );
        let _ = fs::remove_file(p);
    }
    #[test]
    fn session_count_and_recovery() {
        let (p, s) = db("session");
        s.apply_scan(&scan(vec![rec("/music/a.mp3", "a", "A", 600.)]))
            .unwrap();
        let t = s.tracks().unwrap()[0].id;
        let id = s.begin_session(t, 1).unwrap();
        s.update_session(id, 100., None, "").unwrap();
        s.update_session(id, 100., None, "").unwrap();
        s.update_session(id, 240., None, "").unwrap();
        s.update_session(id, 500., Some(2), "done").unwrap();
        s.update_session(id, 500., Some(3), "done").unwrap();
        assert_eq!(s.tracks().unwrap()[0].play_count, 1);
        assert_eq!(s.tracks().unwrap()[0].listen_seconds, 500.);
        let open = s.begin_session(t, 4).unwrap();
        s.update_session(open, 10., None, "").unwrap();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert!(
            s.history(10)
                .unwrap()
                .iter()
                .any(|h| h.id == open && h.reason == "interrupted" && !h.counted)
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
            ]))
            .unwrap();
        let tracks = store.tracks().unwrap();
        let playlist = store.create_playlist("mix").unwrap();
        store
            .add_playlist(
                playlist,
                &[tracks[0].id, tracks[1].id, tracks[0].id, tracks[1].id],
            )
            .unwrap();
        let before = store.playlists().unwrap().remove(0).entries;
        store.remove_tracks(&[tracks[0].id]).unwrap();
        store.move_playlist_entry(before[1].id, 1).unwrap();
        assert_eq!(
            store.playlists().unwrap()[0]
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![before[3].id, before[1].id]
        );
        drop(store);
        let _ = fs::remove_file(path);
    }

    fn legacy_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rivu-{name}-{}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(r#"
            CREATE TABLE schema_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
            INSERT INTO schema_meta VALUES('version',1);
            CREATE TABLE tracks(id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL UNIQUE,
                fingerprint TEXT,file_size INTEGER NOT NULL DEFAULT 0,modified_ns INTEGER NOT NULL DEFAULT 0,
                raw_title TEXT NOT NULL DEFAULT '',raw_artist TEXT NOT NULL DEFAULT '',raw_album TEXT NOT NULL DEFAULT '',
                title_override TEXT,artist_override TEXT,album_override TEXT,duration REAL,
                codec TEXT NOT NULL DEFAULT '',channels INTEGER NOT NULL DEFAULT 0,sample_rate INTEGER NOT NULL DEFAULT 0,
                missing INTEGER NOT NULL DEFAULT 0,play_count INTEGER NOT NULL DEFAULT 0,
                listen_seconds REAL NOT NULL DEFAULT 0,last_played INTEGER);
            CREATE INDEX tracks_fingerprint ON tracks(fingerprint);
            CREATE TABLE playlists(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL);
            CREATE TABLE playlist_entries(id INTEGER PRIMARY KEY AUTOINCREMENT,
                playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
                track_id INTEGER NOT NULL,position INTEGER NOT NULL);
            CREATE INDEX playlist_order ON playlist_entries(playlist_id,position,id);
            CREATE TABLE sessions(id INTEGER PRIMARY KEY AUTOINCREMENT,track_id INTEGER NOT NULL,
                title TEXT NOT NULL DEFAULT '',started_at INTEGER NOT NULL,ended_at INTEGER,
                listened_seconds REAL NOT NULL DEFAULT 0,counted INTEGER NOT NULL DEFAULT 0,
                reason TEXT NOT NULL DEFAULT '');
            CREATE INDEX session_order ON sessions(started_at DESC,id DESC);
            CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            INSERT INTO tracks(id,path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,
                title_override,artist_override,album_override,duration,codec,channels,sample_rate,
                missing,play_count,listen_seconds,last_played)
                VALUES(7,'/music/legacy.wav','legacy',123,456,'Raw','Raw artist','Raw album',
                    'Override','Artist','Album',60,'pcm',2,44100,0,3,90,22);
            INSERT INTO tracks(id,path) VALUES(99,'/music/deleted.wav');
            DELETE FROM tracks WHERE id=99;
            INSERT INTO playlists(id,name) VALUES(4,'Saved');
            INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES(11,4,7,0),(12,4,7,1);
            INSERT INTO sessions(id,track_id,title,started_at,ended_at,listened_seconds,counted,reason)
                VALUES(20,7,'Snapshot',10,22,30,1,'done');
            INSERT INTO settings VALUES('queue','{"tracks":[7,7],"current":1}');
            INSERT INTO settings VALUES('volume','0.6');
        "#).unwrap();
        path
    }

    #[test]
    fn migration_preserves_ids_overrides_history_playlists_and_settings() {
        let path = legacy_db("cue-migration");
        let store = Store::open(&path).unwrap();
        let track = store.tracks().unwrap().remove(0);
        assert_eq!(track.id, 7);
        assert_eq!(
            (&*track.title, &*track.artist, &*track.album),
            ("Override", "Artist", "Album")
        );
        assert_eq!(
            (track.play_count, track.listen_seconds, track.last_played),
            (3, 90.0, Some(22))
        );
        assert_eq!(
            (track.duration, track.channels, track.sample_rate),
            (Some(60.0), 2, 44100)
        );
        assert_eq!(track.codec, "pcm");
        assert!(!track.missing);
        assert!(track.cue.is_none());
        let known = store.known_files().unwrap().remove(0);
        assert_eq!((known.size, known.modified_ns), (123, 456));
        assert_eq!(known.fingerprint.as_deref(), Some("legacy"));
        assert_eq!(known.media.unwrap().title, "Raw");
        assert!(known.cue.is_none());
        let playlist = store.playlists().unwrap().remove(0);
        assert_eq!((playlist.id, playlist.name.as_str()), (4, "Saved"));
        assert_eq!(
            playlist
                .entries
                .iter()
                .map(|e| (e.id, e.track_id))
                .collect::<Vec<_>>(),
            vec![(11, 7), (12, 7)]
        );
        let history = store.history(10).unwrap().remove(0);
        assert_eq!(
            (
                history.id,
                history.track_id,
                history.started_at,
                history.ended_at
            ),
            (20, 7, 10, Some(22))
        );
        assert_eq!(
            (
                history.title.as_str(),
                history.listened_seconds,
                history.counted,
                history.reason.as_str()
            ),
            ("Snapshot", 30.0, true, "done")
        );
        assert_eq!(
            store.get_setting("queue").unwrap().as_deref(),
            Some(r#"{"tracks":[7,7],"current":1}"#)
        );
        assert_eq!(store.get_setting("volume").unwrap().as_deref(), Some("0.6"));
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT value FROM schema_meta WHERE key='version'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        store
            .apply_scan(&scan(vec![rec("/music/new.wav", "new", "New", 60.0)]))
            .unwrap();
        assert!(
            store
                .tracks()
                .unwrap()
                .iter()
                .find(|t| t.path == Path::new("/music/new.wav"))
                .unwrap()
                .id
                > 99
        );
        drop(store);
        assert_eq!(
            Store::open(&path).unwrap().playlists().unwrap()[0].entries[0].track_id,
            7
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn migration_preserves_high_water_when_all_tracks_were_deleted() {
        let path = legacy_db("cue-migration-empty");
        let conn = Connection::open(&path).unwrap();
        conn.execute("DELETE FROM tracks", []).unwrap();
        drop(conn);
        let store = Store::open(&path).unwrap();
        store
            .apply_scan(&scan(vec![rec("/music/new.wav", "new", "New", 60.0)]))
            .unwrap();
        assert!(store.tracks().unwrap()[0].id > 99);
        assert_eq!(store.history(10).unwrap()[0].track_id, 7);
        drop(store);
        let _ = fs::remove_file(path);
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
        let session = store.begin_session(ids[1], 10).unwrap();
        store
            .update_session(session, 35.0, Some(11), "done")
            .unwrap();
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
        assert_eq!((after[1].play_count, after[1].listen_seconds), (1, 35.0));
        for index in [0, 2, 3] {
            assert_eq!(
                (after[index].play_count, after[index].listen_seconds),
                (0, 0.0)
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
        assert_eq!(store.playlists().unwrap()[0].entries.len(), 4);
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
        assert_eq!(store.history(10).unwrap()[0].track_id, ids[1]);
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

    #[test]
    fn failed_migration_rolls_back_schema_rows_and_sequence() {
        let path = legacy_db("cue-migration-rollback");
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE INDEX tracks_path ON settings(value)", [])
            .unwrap();
        drop(conn);
        assert!(Store::open(&path).is_err());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT value FROM schema_meta WHERE key='version'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT title_override FROM tracks WHERE id=7", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "Override"
        );
        assert_eq!(
            conn.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='tracks'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            99
        );
        assert!(
            conn.execute("INSERT INTO tracks(path) VALUES('/music/legacy.wav')", [])
                .is_err()
        );
        conn.execute("DROP INDEX tracks_path", []).unwrap();
        drop(conn);
        assert_eq!(Store::open(&path).unwrap().tracks().unwrap()[0].id, 7);
        let _ = fs::remove_file(path);
    }
}
