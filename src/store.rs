use crate::library::{KnownFile, MediaInfo, ScanResult};
use crate::model::{HistoryEntry, Playlist, PlaylistEntry, Track};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Store {
    conn: Connection,
}
#[derive(Clone)]
struct Existing {
    id: i64,
    path: PathBuf,
    hash: Option<String>,
    missing: bool,
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
        if v > 1 {
            bail!("unsupported schema version {v}");
        }
        if v < 1 {
            c.execute_batch("BEGIN; CREATE TABLE tracks(id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL UNIQUE,fingerprint TEXT,file_size INTEGER NOT NULL DEFAULT 0,modified_ns INTEGER NOT NULL DEFAULT 0,raw_title TEXT NOT NULL DEFAULT '',raw_artist TEXT NOT NULL DEFAULT '',raw_album TEXT NOT NULL DEFAULT '',title_override TEXT,artist_override TEXT,album_override TEXT,duration REAL,codec TEXT NOT NULL DEFAULT '',channels INTEGER NOT NULL DEFAULT 0,sample_rate INTEGER NOT NULL DEFAULT 0,missing INTEGER NOT NULL DEFAULT 0,play_count INTEGER NOT NULL DEFAULT 0,listen_seconds REAL NOT NULL DEFAULT 0,last_played INTEGER); CREATE INDEX tracks_fingerprint ON tracks(fingerprint); CREATE TABLE playlists(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL); CREATE TABLE playlist_entries(id INTEGER PRIMARY KEY AUTOINCREMENT,playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,track_id INTEGER NOT NULL,position INTEGER NOT NULL); CREATE INDEX playlist_order ON playlist_entries(playlist_id,position,id); CREATE TABLE sessions(id INTEGER PRIMARY KEY AUTOINCREMENT,track_id INTEGER NOT NULL,title TEXT NOT NULL DEFAULT '',started_at INTEGER NOT NULL,ended_at INTEGER,listened_seconds REAL NOT NULL DEFAULT 0,counted INTEGER NOT NULL DEFAULT 0,reason TEXT NOT NULL DEFAULT ''); CREATE INDEX session_order ON sessions(started_at DESC,id DESC); UPDATE schema_meta SET value=1 WHERE key='version'; COMMIT;")?;
        }
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL)",
        )?;
        let s = Self { conn: c };
        s.finish_interrupted()?;
        Ok(s)
    }
    pub fn tracks(&self) -> Result<Vec<Track>> {
        let mut q=self.conn.prepare("SELECT id,path,COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),COALESCE(album_override,raw_album),duration,codec,channels,sample_rate,missing,play_count,listen_seconds,last_played FROM tracks ORDER BY id")?;
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
        let mut q=self.conn.prepare("SELECT id,path,file_size,modified_ns,fingerprint,raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate FROM tracks")?;
        Ok(q.query_map([], |r| {
            Ok(KnownFile {
                track_id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                size: r.get::<_, i64>(2)? as u64,
                modified_ns: r.get(3)?,
                fingerprint: r.get(4)?,
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
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut q = tx.prepare("SELECT id,path,fingerprint,missing FROM tracks")?;
        let mut old: Vec<Existing> = q
            .query_map([], |r| {
                Ok(Existing {
                    id: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    hash: r.get(2)?,
                    missing: r.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        let paths: HashSet<PathBuf> = result.records.iter().map(|r| r.path.clone()).collect();
        for o in &mut old {
            if !o.missing
                && result
                    .roots
                    .iter()
                    .any(|x| o.path == *x || o.path.starts_with(x))
                && !paths.contains(&o.path)
            {
                o.missing = true;
                tx.execute("UPDATE tracks SET missing=1 WHERE id=?", [o.id])?;
            }
        }
        let candidate_hashes: HashSet<&str> = result
            .records
            .iter()
            .filter_map(|record| record.fingerprint.as_deref())
            .collect();
        let mut bypath = HashMap::new();
        let mut byhash: HashMap<String, Vec<i64>> = HashMap::new();
        for record in &old {
            bypath.insert(record.path.clone(), record.id);
            if !paths.contains(&record.path)
                && let Some(hash) = &record.hash
                && candidate_hashes.contains(hash.as_str())
                && fs::symlink_metadata(&record.path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                byhash.entry(hash.clone()).or_default().push(record.id);
            }
        }
        let mut seen = HashSet::new();
        for r in &result.records {
            let id = bypath.get(&r.path).copied().or_else(|| {
                r.fingerprint.as_ref().and_then(|h| {
                    byhash.get(h).and_then(|ids| {
                        (ids.len() == 1 && !seen.contains(&ids[0])).then_some(ids[0])
                    })
                })
            });
            let m = &r.media;
            if let Some(id) = id {
                tx.execute("UPDATE tracks SET path=?,fingerprint=?,file_size=?,modified_ns=?,raw_title=?,raw_artist=?,raw_album=?,duration=?,codec=?,channels=?,sample_rate=?,missing=0 WHERE id=?",params![r.path.to_str(),r.fingerprint.as_deref(),r.size as i64,r.modified_ns,m.title,m.artist,m.album,m.duration,m.codec,m.channels as i64,m.sample_rate as i64,id])?;
                seen.insert(id);
            } else {
                tx.execute("INSERT INTO tracks(path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate) VALUES(?,?,?,?,?,?,?,?,?,?,?)",params![r.path.to_str(),r.fingerprint.as_deref(),r.size as i64,r.modified_ns,m.title,m.artist,m.album,m.duration,m.codec,m.channels as i64,m.sample_rate as i64])?;
                seen.insert(tx.last_insert_rowid());
            }
        }
        for o in old {
            if !seen.contains(&o.id)
                && result
                    .roots
                    .iter()
                    .any(|x| o.path == *x || o.path.starts_with(x))
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
}
