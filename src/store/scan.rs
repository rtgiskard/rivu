use super::Store;
use super::rows::{cue_from_row, fingerprint_from_row};
use crate::library::{KnownFile, ScanResult, ScanUncertainty};
use crate::model::{CueSegment, MediaInfo};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// Bump when probing semantics change; fingerprints remain reusable.
const PROBE_VERSION: i64 = 3;

// Keep equality and descendant branches separate so both path indexes can seek.
// A slash-terminated prefix has the exclusive BINARY upper bound ending in '0'.
const SCAN_SCOPE_IDS: &str = "
    SELECT t.id FROM json_each(?1) AS scope
    CROSS JOIN tracks AS t INDEXED BY tracks_directory WHERE t.path=scope.value
    UNION
    SELECT t.id FROM json_each(?2) AS scope
    CROSS JOIN tracks AS t INDEXED BY tracks_directory
    WHERE t.path>=scope.value AND t.path<(rtrim(scope.value,'/')||'0')
    UNION
    SELECT t.id FROM json_each(?1) AS scope
    CROSS JOIN tracks AS t INDEXED BY tracks_cue
    WHERE t.cue_sheet IS NOT NULL AND t.cue_sheet=scope.value
    UNION
    SELECT t.id FROM json_each(?2) AS scope
    CROSS JOIN tracks AS t INDEXED BY tracks_cue
    WHERE t.cue_sheet IS NOT NULL
      AND t.cue_sheet>=scope.value AND t.cue_sheet<(rtrim(scope.value,'/')||'0')";

fn scan_scope(paths: &[PathBuf]) -> Result<(HashSet<&str>, HashSet<String>)> {
    let mut exact = HashSet::with_capacity(paths.len());
    let mut prefixes = HashSet::with_capacity(paths.len());
    for path in paths {
        let normalized = crate::library::logical_path(path)?;
        if normalized != *path {
            bail!(
                "Scan scope is not a normalized absolute path: {}",
                path.display()
            );
        }
        let path = path.to_str().context("Scan scope path is not UTF-8")?;
        exact.insert(path);
        prefixes.insert(format!("{}/", path.trim_end_matches('/')));
    }
    Ok((exact, prefixes))
}

struct Existing {
    id: i64,
    path: PathBuf,
    hash: Option<String>,
    cue: Option<CueSegment>,
    independent: bool,
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

#[derive(Hash, PartialEq, Eq)]
enum RelocationKey<'a> {
    File(&'a str),
    Cue(&'a str, u32, u64, Option<u64>),
}

fn relocation_key<'a>(hash: &'a str, cue: Option<&CueSegment>) -> RelocationKey<'a> {
    match cue {
        Some(cue) => RelocationKey::Cue(hash, cue.number, cue.start_frame, cue.end_frame),
        None => RelocationKey::File(hash),
    }
}

#[cfg(test)]
const KNOWN_COLUMNS: &str =
    "id,path,file_size,modified_ns,fingerprint,raw_title,raw_artist,raw_album,
    duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,
    cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,
    bits_per_sample,release_date";

fn known_file_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KnownFile> {
    Ok(KnownFile {
        track_id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        size: r.get::<_, i64>(2)? as u64,
        modified_ns: r.get(3)?,
        fingerprint: fingerprint_from_row(r, 4)?,
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
}

impl Store {
    /// Read one reusable source snapshot, preferring an ordinary file identity.
    /// Both branches seek existing path indexes; no unrelated row is decoded.
    pub fn known_source(&self, path: &Path) -> Result<Option<KnownFile>> {
        let path = path.to_str().context("Scan source path is not UTF-8")?;
        let mut q = self.conn.prepare_cached(
            "SELECT id,path,file_size,modified_ns,fingerprint,raw_title,raw_artist,raw_album,
                    duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,
                    cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,
                    bits_per_sample,release_date
             FROM tracks WHERE id=COALESCE(
                (SELECT id FROM tracks INDEXED BY tracks_directory
                 WHERE path=?1 AND cue_sheet IS NULL ORDER BY id LIMIT 1),
                (SELECT id FROM tracks INDEXED BY tracks_directory
                 WHERE path=?1 ORDER BY id LIMIT 1)
             )",
        )?;
        Ok(q.query_row([path], known_file_from_row).optional()?)
    }

    /// Inspect stored raw metadata for test fixtures, including every CUE identity.
    #[cfg(test)]
    pub(crate) fn stored_scan_files(&self, paths: &[PathBuf]) -> Result<Vec<KnownFile>> {
        let (exact, prefixes) = scan_scope(paths)?;
        let mut q = self.conn.prepare(&format!(
            "SELECT {KNOWN_COLUMNS} FROM tracks WHERE id IN ({SCAN_SCOPE_IDS}) ORDER BY id"
        ))?;
        Ok(q.query_map(
            params![
                serde_json::to_string(&exact)?,
                serde_json::to_string(&prefixes)?
            ],
            known_file_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn apply_scan(&self, result: &ScanResult) -> Result<bool> {
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
        let (mut exact, prefixes) = scan_scope(&result.roots)?;
        exact.extend(result.records.iter().map(|record| {
            record
                .cue
                .as_ref()
                .map_or(record.path.as_path(), |cue| cue.sheet.as_path())
                .to_str()
                .expect("scan record paths validated above")
        }));
        let mut selected = HashMap::new();
        let read_existing = |r: &rusqlite::Row<'_>| {
            Ok(Existing {
                id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                hash: fingerprint_from_row(r, 2)?,
                cue: cue_from_row(r, 3)?,
                independent: r.get(7)?,
            })
        };
        let mut q = tx.prepare(&format!(
            "SELECT id,path,fingerprint,cue_sheet,cue_number,cue_start_frame,cue_end_frame,relocation_independent
             FROM tracks WHERE id IN ({SCAN_SCOPE_IDS})"
        ))?;
        for row in q.query_map(
            params![
                serde_json::to_string(&exact)?,
                serde_json::to_string(&prefixes)?
            ],
            read_existing,
        )? {
            let row = row?;
            selected.insert(row.id, row);
        }
        drop(q);
        let candidate_hashes: HashSet<&str> = result
            .records
            .iter()
            .filter_map(|record| record.fingerprint.as_deref())
            .collect();
        let ordinary_candidate_hashes: HashSet<&str> = result
            .records
            .iter()
            .filter(|record| record.cue.is_none())
            .filter_map(|record| record.fingerprint.as_deref())
            .collect();
        let mut q = tx.prepare(
            "SELECT id,path,fingerprint,cue_sheet,cue_number,cue_start_frame,cue_end_frame,relocation_independent
             FROM tracks INDEXED BY tracks_fingerprint WHERE fingerprint=?1",
        )?;
        for hash in &candidate_hashes {
            let hash = blake3::Hash::from_hex(hash).context("Invalid scan BLAKE3 fingerprint")?;
            let mut rows = q.query([hash.as_bytes().as_slice()])?;
            while let Some(row) = rows.next()? {
                let id = row.get::<_, i64>(0)?;
                if let std::collections::hash_map::Entry::Vacant(entry) = selected.entry(id) {
                    entry.insert(read_existing(row)?);
                }
            }
        }
        drop(q);
        // The original table scan visited integer primary keys in row order.
        // Preserve that order for identities and global relocation candidates.
        let mut old: Vec<Existing> = selected.into_values().collect();
        old.sort_unstable_by_key(|record| record.id);
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
        // None records ambiguity without allocating a candidate vector per segment.
        let mut candidates_by_key: HashMap<RelocationKey<'_>, Option<&Existing>> = HashMap::new();
        let mut absent_sheets = HashMap::new();
        let mut absent_ordinary = HashSet::new();
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
                && match &record.cue {
                    Some(cue) => *absent_sheets.entry(cue.sheet.as_path()).or_insert_with(|| {
                        fs::symlink_metadata(owner)
                            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    }),
                    None => fs::symlink_metadata(owner)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                }
            {
                if record.cue.is_none() && ordinary_candidate_hashes.contains(hash) {
                    absent_ordinary.insert(record.id);
                }
                if record.cue.is_some() || !record.independent {
                    candidates_by_key
                        .entry(relocation_key(hash, record.cue.as_ref()))
                        .and_modify(|candidate| *candidate = None)
                        .or_insert(Some(record));
                }
            }
            byidentity.insert(key, record.id);
        }
        // Ordinary moves require uniqueness on both sides. Resolve exact paths
        // first; never let scan order choose one of several newly found copies.
        let mut new_ordinary: HashMap<&str, Option<&Path>> = HashMap::new();
        for record in &result.records {
            if record.cue.is_none()
                && !byidentity.contains_key(&identity(&record.path, None))
                && let Some(hash) = record.fingerprint.as_deref()
            {
                new_ordinary
                    .entry(hash)
                    .and_modify(|path| {
                        if *path != Some(record.path.as_path()) {
                            *path = None;
                        }
                    })
                    .or_insert(Some(record.path.as_path()));
            }
        }
        let mut seen = HashSet::new();
        tx.execute_batch(
            "CREATE TEMP TABLE scan_stage(
                resolved_id INTEGER,
                path TEXT NOT NULL,
                fingerprint BLOB,
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
                    if r.cue.is_none() && !new_ordinary.get(hash).is_some_and(Option::is_some) {
                        return None;
                    }
                    let candidate = candidates_by_key
                        .get(&relocation_key(hash, r.cue.as_ref()))
                        .copied()
                        .flatten()?;
                    (!seen.contains(&candidate.id)).then_some(candidate.id)
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
            let fingerprint = r
                .fingerprint
                .as_deref()
                .map(blake3::Hash::from_hex)
                .transpose()
                .context("Invalid scan BLAKE3 fingerprint")?;
            stage.execute(params![
                id,
                r.path.to_str(),
                fingerprint.as_ref().map(|hash| hash.as_bytes().as_slice()),
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
        let mut changed_tracks = tx.execute(
            "UPDATE tracks SET
                path=s.path,fingerprint=s.fingerprint,file_size=s.file_size,modified_ns=s.modified_ns,
                raw_title=s.raw_title,raw_artist=s.raw_artist,raw_album=s.raw_album,duration=s.duration,
                codec=s.codec,channels=s.channels,sample_rate=s.sample_rate,cue_sheet=s.cue_sheet,
                cue_number=s.cue_number,cue_start_frame=s.cue_start_frame,cue_end_frame=s.cue_end_frame,
                missing=0,probe_version=s.probe_version,bitrate_bps=s.bitrate_bps,
                track_number=s.track_number,disc_number=s.disc_number,bits_per_sample=s.bits_per_sample,
                release_date=s.release_date
             FROM temp.scan_stage AS s WHERE tracks.id=s.resolved_id AND (
                tracks.path IS NOT s.path OR tracks.fingerprint IS NOT s.fingerprint
                OR tracks.file_size IS NOT s.file_size OR tracks.modified_ns IS NOT s.modified_ns
                OR tracks.raw_title IS NOT s.raw_title OR tracks.raw_artist IS NOT s.raw_artist
                OR tracks.raw_album IS NOT s.raw_album OR tracks.duration IS NOT s.duration
                OR tracks.codec IS NOT s.codec OR tracks.channels IS NOT s.channels
                OR tracks.sample_rate IS NOT s.sample_rate OR tracks.cue_sheet IS NOT s.cue_sheet
                OR tracks.cue_number IS NOT s.cue_number
                OR tracks.cue_start_frame IS NOT s.cue_start_frame
                OR tracks.cue_end_frame IS NOT s.cue_end_frame OR tracks.missing IS NOT 0
                OR tracks.probe_version IS NOT s.probe_version
                OR tracks.bitrate_bps IS NOT s.bitrate_bps
                OR tracks.track_number IS NOT s.track_number OR tracks.disc_number IS NOT s.disc_number
                OR tracks.bits_per_sample IS NOT s.bits_per_sample
                OR tracks.release_date IS NOT s.release_date
             )",
            [],
        )?;
        changed_tracks += tx.execute(
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
        let mut mark_missing =
            tx.prepare("UPDATE tracks SET missing=1 WHERE id=?1 AND missing=0")?;
        for o in &old {
            let owner = o
                .cue
                .as_ref()
                .map_or(o.path.as_path(), |cue| cue.sheet.as_path());
            if !seen.contains(&o.id)
                && !(o.cue.is_none() && suppressed.contains(o.path.as_path()))
                && (result
                    .roots
                    .iter()
                    .any(|root| owner == root || owner.starts_with(root))
                    || absent_ordinary.contains(&o.id))
                && !unconfirmed(o)
            {
                changed_tracks += mark_missing.execute([o.id])?;
            }
        }
        drop(mark_missing);
        tx.commit()?;
        if changed_tracks != 0 {
            self.track_cache.borrow_mut().entries.clear();
        }
        Ok(changed_tracks != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::{PROBE_VERSION, Store};
    use crate::library::{ScanRecord, ScanResult};
    use crate::model::{CueSegment, MediaInfo};
    use std::path::Path;

    fn record() -> ScanRecord {
        ScanRecord {
            path: "/music/tagged.wav".into(),
            cue: None,
            size: 1024,
            modified_ns: 42,
            fingerprint: Some(blake3::hash(b"fingerprint").to_hex().to_string()),
            media: MediaInfo {
                title: "Title".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                duration: Some(60.0),
                codec: "pcm".into(),
                channels: 2,
                sample_rate: 48000,
                bitrate_bps: Some(1_536_000),
                track_number: Some(1),
                disc_number: Some(1),
                bits_per_sample: Some(16),
                release_date: Some("2024".into()),
            },
        }
    }

    fn queued(record: ScanRecord) -> (Store, ScanResult, i64) {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let scan = ScanResult {
            roots: vec!["/music".into()],
            records: vec![record],
            ..ScanResult::default()
        };
        assert!(store.apply_scan(&scan).unwrap());
        store.enqueue_sources(&["/music".into()], &[]).unwrap();
        let id = store.queue_page(0, 10).unwrap().rows[0].track_id;
        (store, scan, id)
    }

    #[test]
    fn identical_rescans_keep_queue_content_and_revision_stable() {
        let (store, scan, id) = queued(record());
        store.set_favorite(&[id], true).unwrap();
        store.mark_played(id, 123).unwrap();
        store.increment_play_count(id).unwrap();
        store
            .edit_track(id, Some("Override"), None, None, None)
            .unwrap();
        let page = store.queue_page(0, 10).unwrap();
        store.set_queue_current(Some(page.rows[0].id)).unwrap();
        let state = store.queue_state().unwrap();
        let track = store.track(id).unwrap().unwrap();
        for _ in 0..2 {
            assert!(!store.apply_scan(&scan).unwrap());
            assert_eq!(store.queue_state().unwrap(), state);
            assert_eq!(store.queue_page(0, 10).unwrap(), page);
            assert_eq!(store.track(id).unwrap().unwrap(), track);
        }
    }

    #[test]
    fn changed_scan_metadata_invalidates_queue_without_losing_entry_identity() {
        let (store, mut scan, id) = queued(record());
        let before = store.queue_page(0, 10).unwrap().rows.remove(0);
        let state = store.queue_state().unwrap();
        scan.records[0].media.title = "Changed title".into();
        scan.records[0].media.artist = "Changed artist".into();
        scan.records[0].media.album = "Changed album".into();
        scan.records[0].media.codec = "flac".into();
        store.apply_scan(&scan).unwrap();
        assert!(store.queue_state().unwrap().revision > state.revision);
        let after = store.queue_page(0, 10).unwrap().rows.remove(0);
        assert_eq!(
            (after.id, after.track_id, after.position),
            (before.id, id, before.position)
        );
        assert_eq!(after.track.title, "Changed title");
        assert_eq!(after.track.artist, "Changed artist");
        assert_eq!(after.track.album, "Changed album");
        assert_eq!(after.track.codec, "flac");
        let state = store.queue_state().unwrap();
        store.apply_scan(&scan).unwrap();
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(0, 10).unwrap().rows[0], after);
    }

    #[test]
    fn raw_metadata_changes_under_overrides_persist_without_invalidating_queue() {
        let (store, mut scan, id) = queued(record());
        store
            .edit_track(
                id,
                Some("Custom title"),
                Some("Custom artist"),
                Some("Custom album"),
                None,
            )
            .unwrap();
        let page = store.queue_page(0, 10).unwrap();
        let state = store.queue_state().unwrap();
        scan.records[0].media.title = "New raw title".into();
        scan.records[0].media.artist = "New raw artist".into();
        scan.records[0].media.album = "New raw album".into();
        store.apply_scan(&scan).unwrap();
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(0, 10).unwrap(), page);
        let known = store.stored_scan_files(&scan.roots).unwrap();
        let raw = known[0].media.as_ref().unwrap();
        assert_eq!(raw.title, "New raw title");
        assert_eq!(raw.artist, "New raw artist");
        assert_eq!(raw.album, "New raw album");
        let track = store.track(id).unwrap().unwrap();
        assert_eq!(track.title, "Custom title");
        assert_eq!(track.artist, "Custom artist");
        assert_eq!(track.album, "Custom album");
    }

    #[test]
    fn equal_to_raw_and_repeated_overrides_keep_queue_revision_stable() {
        let (store, mut scan, id) = queued(record());
        let page = store.queue_page(0, 10).unwrap();
        let state = store.queue_state().unwrap();
        for (title, artist, album) in [
            (Some("Title"), None, None),
            (None, Some("Artist"), None),
            (None, None, Some("Album")),
            (Some("Title"), Some("Artist"), Some("Album")),
        ] {
            for _ in 0..2 {
                store.edit_track(id, title, artist, album, None).unwrap();
                assert_eq!(store.queue_state().unwrap(), state);
                assert_eq!(store.queue_page(0, 10).unwrap(), page);
            }
        }
        // A later rescan proves equal overrides were retained, not discarded.
        scan.records[0].media.title = "Changed raw title".into();
        scan.records[0].media.artist = "Changed raw artist".into();
        scan.records[0].media.album = "Changed raw album".into();
        store.apply_scan(&scan).unwrap();
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(0, 10).unwrap(), page);
    }

    #[test]
    fn displayed_overrides_invalidate_queue_including_empty_strings() {
        let (store, _, id) = queued(record());
        for (title, artist, album) in [
            (Some("Custom title"), None, None),
            (None, Some("Custom artist"), None),
            (None, None, Some("Custom album")),
            (Some(""), None, None),
            (None, Some(""), None),
            (None, None, Some("")),
        ] {
            let before = store.queue_page(0, 10).unwrap().rows.remove(0);
            let state = store.queue_state().unwrap();
            store.edit_track(id, title, artist, album, None).unwrap();
            assert!(store.queue_state().unwrap().revision > state.revision);
            let after = store.queue_page(0, 10).unwrap().rows.remove(0);
            assert_eq!(
                (after.id, after.track_id, after.position),
                (before.id, id, before.position)
            );
            assert_eq!(after.track.title, title.unwrap_or(&before.track.title));
            assert_eq!(after.track.artist, artist.unwrap_or(&before.track.artist));
            assert_eq!(after.track.album, album.unwrap_or(&before.track.album));
            let state = store.queue_state().unwrap();
            store.edit_track(id, title, artist, album, None).unwrap();
            assert_eq!(store.queue_state().unwrap(), state);
            assert_eq!(store.queue_page(0, 10).unwrap().rows[0], after);
        }
    }

    #[test]
    fn missing_transitions_invalidate_once_and_recovery_preserves_queue_entry() {
        let (store, scan, id) = queued(record());
        let page = store.queue_page(0, 10).unwrap();
        let initial = store.queue_state().unwrap();
        let missing = ScanResult {
            roots: scan.roots.clone(),
            ..ScanResult::default()
        };
        store.apply_scan(&missing).unwrap();
        let state = store.queue_state().unwrap();
        assert!(state.revision > initial.revision);
        let row = store.queue_page(0, 10).unwrap().rows.remove(0);
        assert_eq!(row.id, page.rows[0].id);
        assert_eq!(row.track_id, id);
        assert!(row.track.missing);
        store.apply_scan(&missing).unwrap();
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(0, 10).unwrap().rows[0], row);
        store.apply_scan(&scan).unwrap();
        let recovered = store.queue_state().unwrap();
        assert!(recovered.revision > state.revision);
        assert_eq!(store.queue_page(0, 10).unwrap(), page);
        store.apply_scan(&scan).unwrap();
        assert_eq!(store.queue_state().unwrap(), recovered);
        assert_eq!(store.queue_page(0, 10).unwrap(), page);
    }

    #[test]
    fn optional_scan_metadata_clears_and_restores_queue_values() {
        let (store, mut scan, id) = queued(record());
        let original = scan.records[0].clone();
        let state = store.queue_state().unwrap();
        scan.records[0].media.duration = None;
        scan.records[0].media.bitrate_bps = None;
        scan.records[0].fingerprint = None;
        store.apply_scan(&scan).unwrap();
        let cleared = store.queue_state().unwrap();
        assert!(cleared.revision > state.revision);
        let row = store.queue_page(0, 10).unwrap().rows.remove(0);
        assert_eq!(row.track.duration, None);
        assert_eq!(row.track.bitrate_bps, None);
        assert_eq!(store.track(id).unwrap().unwrap().fingerprint, None);
        store.apply_scan(&scan).unwrap();
        assert_eq!(store.queue_state().unwrap(), cleared);
        assert_eq!(store.queue_page(0, 10).unwrap().rows[0], row);
        scan.records[0] = original;
        store.apply_scan(&scan).unwrap();
        assert!(store.queue_state().unwrap().revision > cleared.revision);
        let row = store.queue_page(0, 10).unwrap().rows.remove(0);
        assert_eq!(row.track.duration, Some(60.0));
        assert_eq!(row.track.bitrate_bps, Some(1_536_000));
    }

    #[test]
    fn scan_fields_outside_queue_projection_refresh_without_invalidating_queue() {
        let mut record = record();
        record.cue = Some(CueSegment {
            sheet: "/music/album.cue".into(),
            number: 1,
            start_frame: 0,
            end_frame: Some(4500),
        });
        let (store, mut scan, id) = queued(record);
        let page = store.queue_page(0, 10).unwrap();
        let state = store.queue_state().unwrap();
        let record = &mut scan.records[0];
        record.path = "/music/renamed.wav".into();
        record.fingerprint = None;
        record.size += 1;
        record.modified_ns += 1;
        record.media.channels = 1;
        record.media.sample_rate = 44100;
        record.media.track_number = None;
        record.media.disc_number = None;
        record.media.bits_per_sample = None;
        record.media.release_date = None;
        let cue = record.cue.as_mut().unwrap();
        cue.start_frame = 75;
        cue.end_frame = None;
        store.apply_scan(&scan).unwrap();
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(0, 10).unwrap(), page);
        let track = store.track(id).unwrap().unwrap();
        assert_eq!(track.path, scan.records[0].path);
        assert_eq!(track.fingerprint, None);
        assert_eq!(track.cue, scan.records[0].cue);
        assert_eq!((track.channels, track.sample_rate), (1, 44100));
        assert_eq!(
            (track.track_number, track.disc_number, track.bits_per_sample),
            (None, None, None)
        );
        assert_eq!(track.release_date, None);
        let known = store.stored_scan_files(&scan.roots).unwrap().remove(0);
        assert_eq!(known.size, scan.records[0].size);
        assert_eq!(known.modified_ns, scan.records[0].modified_ns);
        // A stale probe must refresh even when every other assigned field matches.
        store
            .conn
            .execute(
                "UPDATE tracks SET probe_version=?1 WHERE id=?2",
                [PROBE_VERSION - 1, id],
            )
            .unwrap();
        assert!(
            store.stored_scan_files(&scan.roots).unwrap()[0]
                .media
                .is_none()
        );
        store.apply_scan(&scan).unwrap();
        assert!(
            store.stored_scan_files(&scan.roots).unwrap()[0]
                .media
                .is_some()
        );
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(0, 10).unwrap(), page);
    }

    #[test]
    fn unqueued_metadata_changes_do_not_invalidate_queue() {
        let (store, mut scan, _) = queued(record());
        let mut other = record();
        other.path = "/music/other.wav".into();
        other.fingerprint = Some(blake3::hash(b"other").to_hex().to_string());
        scan.records.push(other);
        store.apply_scan(&scan).unwrap();
        let page = store.queue_page(0, 10).unwrap();
        let state = store.queue_state().unwrap();
        scan.records[1].media.title = "Unqueued title".into();
        store.apply_scan(&scan).unwrap();
        let id = store
            .track_id_for_source(&scan.records[1].path, None)
            .unwrap()
            .unwrap();
        assert_eq!(store.track(id).unwrap().unwrap().title, "Unqueued title");
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(0, 10).unwrap(), page);
    }

    #[test]
    fn source_metadata_prefers_whole_file_and_falls_back_to_cue_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selected.wav");
        std::fs::write(&path, []).unwrap();
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut whole = record();
        whole.path = path.clone();
        let mut segment = whole.clone();
        segment.cue = Some(CueSegment {
            sheet: directory.path().join("album.cue"),
            number: 1,
            start_frame: 0,
            end_frame: Some(4500),
        });
        let mut records = vec![segment, whole];
        for index in 0..128 {
            let mut unrelated = record();
            unrelated.path = directory.path().join(format!("other/{index}.wav"));
            unrelated.fingerprint = Some(
                blake3::hash(index.to_string().as_bytes())
                    .to_hex()
                    .to_string(),
            );
            records.push(unrelated);
        }
        assert!(
            store
                .apply_scan(&ScanResult {
                    records,
                    ..ScanResult::default()
                })
                .unwrap()
        );
        let known = store.known_source(&path).unwrap().unwrap();
        assert!(known.cue.is_none());
        assert_eq!(known.path, path);
        assert!(
            store
                .known_source(&directory.path().join("missing.wav"))
                .unwrap()
                .is_none()
        );
        store
            .conn
            .execute("DELETE FROM tracks WHERE id=?1", [known.track_id])
            .unwrap();
        let segment = store.known_source(&path).unwrap().unwrap();
        assert!(segment.cue.is_some());
        assert_eq!(segment.path, path);
        assert_eq!(segment.fingerprint, known.fingerprint);
    }

    #[test]
    fn tiny_scan_relocates_global_identity_without_touching_unrelated_missing_rows() {
        let directory = tempfile::tempdir().unwrap();
        let old_path = directory.path().join("old/source.wav");
        let requested = directory.path().join("tiny");
        let mut original = record();
        original.path = old_path.clone();
        let mut unrelated = record();
        unrelated.path = directory.path().join("tiny-neighbor/absent.wav");
        unrelated.fingerprint = Some(blake3::hash(b"unrelated").to_hex().to_string());
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&ScanResult {
                records: vec![original.clone(), unrelated.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        let id = store.track_id_for_source(&old_path, None).unwrap().unwrap();
        let unrelated_id = store
            .track_id_for_source(&unrelated.path, None)
            .unwrap()
            .unwrap();
        store.set_favorite(&[id], true).unwrap();
        original.path = requested.join("音楽.wav");
        let scan = ScanResult {
            roots: vec![requested.clone()],
            records: vec![original.clone()],
            ..ScanResult::default()
        };
        assert!(store.apply_scan(&scan).unwrap());
        assert_eq!(
            store.track_id_for_source(&original.path, None).unwrap(),
            Some(id)
        );
        assert!(store.track(id).unwrap().unwrap().favorite);
        assert!(!store.track(unrelated_id).unwrap().unwrap().missing);
        assert!(!store.apply_scan(&scan).unwrap());
        let absent = ScanResult {
            roots: vec![requested],
            ..ScanResult::default()
        };
        assert!(store.apply_scan(&absent).unwrap());
        assert!(store.track(id).unwrap().unwrap().missing);
        assert!(!store.track(unrelated_id).unwrap().unwrap().missing);
        assert!(!store.apply_scan(&absent).unwrap());
        assert!(store.apply_scan(&scan).unwrap());
        assert!(!store.track(id).unwrap().unwrap().missing);
    }

    #[test]
    fn uncertain_global_candidate_and_failed_scan_preserve_existing_identity() {
        let directory = tempfile::tempdir().unwrap();
        let mut original = record();
        original.path = directory.path().join("old/source.wav");
        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&ScanResult {
                records: vec![original.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        let id = store
            .track_id_for_source(&original.path, None)
            .unwrap()
            .unwrap();
        store.enqueue_sources(&[], &[id]).unwrap();
        let before = store.track(id).unwrap().unwrap();
        let mut replacement = original.clone();
        replacement.path = directory.path().join("new/source.wav");
        let mut scan = ScanResult {
            roots: vec![replacement.path.clone()],
            records: vec![replacement.clone()],
            uncertainties: vec![crate::library::ScanUncertainty::File(original.path.clone())],
            ..ScanResult::default()
        };
        assert!(store.apply_scan(&scan).unwrap());
        assert_eq!(store.track(id).unwrap().unwrap(), before);
        let new_id = store
            .track_id_for_source(&replacement.path, None)
            .unwrap()
            .unwrap();
        assert_ne!(id, new_id);
        let queue = store.queue_state().unwrap();
        scan.records[0].media.title = "Must roll back".into();
        let mut invalid = record();
        invalid.path = directory.path().join("new/invalid.wav");
        invalid.fingerprint = Some("invalid fingerprint".into());
        scan.records.push(invalid);
        assert!(store.apply_scan(&scan).is_err());
        assert_eq!(
            store.track(new_id).unwrap().unwrap().title,
            replacement.media.title
        );
        assert_eq!(store.track(id).unwrap().unwrap(), before);
        assert_eq!(store.queue_state().unwrap(), queue);
        store
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER reject_scan_insert BEFORE INSERT ON tracks
             WHEN NEW.raw_title='Reject insert'
             BEGIN SELECT RAISE(ABORT,'rejected scan insert'); END",
            )
            .unwrap();
        let mut rejected = scan.records.pop().unwrap();
        rejected.fingerprint = Some(blake3::hash(b"rejected").to_hex().to_string());
        rejected.media.title = "Reject insert".into();
        original.media.title = "Queued update must roll back".into();
        scan.records.extend([original, rejected]);
        assert!(store.apply_scan(&scan).is_err());
        assert_eq!(
            store.track(new_id).unwrap().unwrap().title,
            replacement.media.title
        );
        assert_eq!(store.track(id).unwrap().unwrap(), before);
        assert_eq!(store.queue_state().unwrap(), queue);
    }

    #[test]
    fn one_missing_ordinary_source_and_two_new_copies_preserve_old_id_in_either_scan_order() {
        for reverse in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(Path::new(":memory:")).unwrap();
            let mut original = record();
            original.path = directory.path().join("old/source.wav");
            store
                .apply_scan(&ScanResult {
                    records: vec![original.clone()],
                    ..ScanResult::default()
                })
                .unwrap();
            let id = store
                .track_id_for_source(&original.path, None)
                .unwrap()
                .unwrap();
            store.set_favorite(&[id], true).unwrap();
            store.enqueue_tracks(&[id]).unwrap();
            let queue_id = store.queue_ids().unwrap()[0];
            let mut first = original.clone();
            first.path = directory.path().join("new/first.wav");
            let mut second = original.clone();
            second.path = directory.path().join("new/second.wav");
            let mut records = vec![first.clone(), second.clone()];
            if reverse {
                records.reverse();
            }
            let scan = ScanResult {
                roots: vec![directory.path().join("new")],
                records,
                ..ScanResult::default()
            };
            store.apply_scan(&scan).unwrap();
            let retained = store.track(id).unwrap().unwrap();
            assert_eq!(retained.path, original.path);
            assert!(retained.favorite);
            assert!(retained.missing);
            let first_id = store
                .track_id_for_source(&first.path, None)
                .unwrap()
                .unwrap();
            let second_id = store
                .track_id_for_source(&second.path, None)
                .unwrap()
                .unwrap();
            assert_ne!(first_id, id);
            assert_ne!(second_id, id);
            assert_ne!(first_id, second_id);
            assert_eq!(store.queue_ids().unwrap(), [queue_id]);
            let groups = store.duplicate_groups_page(0, 10).unwrap();
            assert_eq!(groups.rows[0].missing_members, 1);
            assert_eq!(groups.rows[0].present_members, 2);
            assert!(!store.apply_scan(&scan).unwrap());
            assert_eq!(store.track(id).unwrap().unwrap(), retained);
        }
    }

    #[test]
    fn two_missing_ordinary_sources_and_one_new_target_preserve_both_source_states() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut first = record();
        first.path = directory.path().join("old/first.wav");
        let mut second = first.clone();
        second.path = directory.path().join("old/second.wav");
        store
            .apply_scan(&ScanResult {
                records: vec![first.clone(), second.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        let first_id = store
            .track_id_for_source(&first.path, None)
            .unwrap()
            .unwrap();
        let second_id = store
            .track_id_for_source(&second.path, None)
            .unwrap()
            .unwrap();
        store
            .edit_track(first_id, Some("Owned title"), None, None, None)
            .unwrap();
        store.increment_play_count(second_id).unwrap();
        let mut target = first.clone();
        target.path = directory.path().join("new/target.wav");
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().join("new")],
                records: vec![target.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        let target_id = store
            .track_id_for_source(&target.path, None)
            .unwrap()
            .unwrap();
        assert_ne!(target_id, first_id);
        assert_ne!(target_id, second_id);
        assert_eq!(store.track(first_id).unwrap().unwrap().title, "Owned title");
        assert_eq!(store.track(second_id).unwrap().unwrap().play_count, 1);
        assert!(store.track(first_id).unwrap().unwrap().missing);
        assert!(store.track(second_id).unwrap().unwrap().missing);
        let prepared = store.prepare_relocation(first_id, target_id).unwrap();
        store.commit_relocation(&prepared).unwrap();
        assert_eq!(store.track(first_id).unwrap().unwrap().path, target.path);
        assert_eq!(store.track(first_id).unwrap().unwrap().title, "Owned title");
        assert_eq!(store.track(second_id).unwrap().unwrap().path, second.path);
        assert_eq!(store.track(second_id).unwrap().unwrap().play_count, 1);
    }

    #[test]
    fn exact_ordinary_identities_do_not_count_as_new_relocation_targets() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut original = record();
        original.path = directory.path().join("old/source.wav");
        let mut existing = original.clone();
        existing.path = directory.path().join("new/existing.wav");
        store
            .apply_scan(&ScanResult {
                records: vec![original.clone(), existing.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        let source_id = store
            .track_id_for_source(&original.path, None)
            .unwrap()
            .unwrap();
        let existing_id = store
            .track_id_for_source(&existing.path, None)
            .unwrap()
            .unwrap();
        original.path = directory.path().join("new/moved.wav");
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().join("new")],
                records: vec![existing.clone(), original.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        assert_eq!(
            store.track_id_for_source(&original.path, None).unwrap(),
            Some(source_id)
        );
        assert_eq!(
            store.track_id_for_source(&existing.path, None).unwrap(),
            Some(existing_id)
        );
        assert_eq!(store.track_count().unwrap(), 2);
    }

    #[test]
    fn independent_ordinary_identity_is_never_automatically_reassigned_by_later_scan() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut original = record();
        original.path = directory.path().join("old/source.wav");
        store
            .apply_scan(&ScanResult {
                records: vec![original.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        let id = store
            .track_id_for_source(&original.path, None)
            .unwrap()
            .unwrap();
        store.keep_independent(id).unwrap();
        let mut copy = original.clone();
        copy.path = directory.path().join("new/copy.wav");
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().join("new")],
                records: vec![copy.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        assert_ne!(
            store.track_id_for_source(&copy.path, None).unwrap(),
            Some(id)
        );
        assert_eq!(store.track(id).unwrap().unwrap().path, original.path);
        assert!(store.track(id).unwrap().unwrap().missing);
        store
            .apply_scan(&ScanResult {
                records: vec![original.clone(), copy],
                ..ScanResult::default()
            })
            .unwrap();
        assert_eq!(
            store.track_id_for_source(&original.path, None).unwrap(),
            Some(id)
        );
        let members = store
            .duplicate_members_page(original.fingerprint.as_deref().unwrap(), 0, 10)
            .unwrap();
        assert!(
            members
                .rows
                .iter()
                .find(|member| member.track.id == id)
                .unwrap()
                .independent
        );
    }
}
