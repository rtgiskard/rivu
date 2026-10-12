use super::Store;
use super::rows::cue_from_row;
use crate::library::{KnownFile, ScanResult, ScanUncertainty};
use crate::model::{CueSegment, MediaInfo};
use anyhow::{Context, Result, bail};
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// Bump when probing semantics change; fingerprints remain reusable.
const PROBE_VERSION: i64 = 3;

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

impl Store {
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
        let mut excluded_directories: HashSet<(&Path, &Path)> = result
            .skipped_directories
            .iter()
            .map(|entry| {
                let root = result
                    .roots
                    .get(entry.root_index)
                    .context("Skipped directory references an unknown scan root")?;
                Ok((root.as_path(), entry.path.as_path()))
            })
            .collect::<Result<_>>()?;
        let mut uncertain_files = HashSet::new();
        let mut uncertain_sheets = HashSet::new();
        for uncertainty in &result.uncertainties {
            match uncertainty {
                ScanUncertainty::Root { root_index } => {
                    let root = result
                        .roots
                        .get(*root_index)
                        .context("Uncertain area references an unknown scan root")?;
                    excluded_directories.insert((root.as_path(), root.as_path()));
                }
                ScanUncertainty::Subtree { root_index, path } => {
                    let root = result
                        .roots
                        .get(*root_index)
                        .context("Uncertain area references an unknown scan root")?;
                    excluded_directories.insert((root.as_path(), path.as_path()));
                }
                ScanUncertainty::File(path) => {
                    uncertain_files.insert(path.as_path());
                }
                ScanUncertainty::CueSheet(path) => {
                    uncertain_sheets.insert(path.as_path());
                }
            }
        }
        let requested_roots: HashSet<&Path> = result.roots.iter().map(PathBuf::as_path).collect();
        let unconfirmed = |record: &Existing| {
            // Truncation can omit a failed CUE source outside the requested roots:
            // conservatively disable all relocation as well as missing detection.
            if result.scan_incomplete {
                return true;
            }
            let owner = match &record.cue {
                Some(cue) => {
                    if uncertain_sheets.contains(cue.sheet.as_path()) {
                        return true;
                    }
                    cue.sheet.as_path()
                }
                None => {
                    if uncertain_files.contains(record.path.as_path()) {
                        return true;
                    }
                    record.path.as_path()
                }
            };
            // Only directory exclusions produced by the nearest explicit scan apply.
            let Some(root) = owner
                .ancestors()
                .find(|ancestor| requested_roots.contains(ancestor))
            else {
                return false;
            };
            owner
                .ancestors()
                .any(|ancestor| excluded_directories.contains(&(root, ancestor)))
        };
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
                && !unconfirmed(record)
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
            // Missing detection visits only pre-scan IDs. New inserts cannot
            // belong to that set; resolved old IDs are already accounted for.
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
                && !unconfirmed(o)
            {
                mark_missing.execute([o.id])?;
            }
        }
        drop(mark_missing);
        tx.commit()?;
        Ok(())
    }
}
