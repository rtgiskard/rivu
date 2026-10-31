use super::Store;
use super::rows::{cue_from_row, fingerprint_from_row};
use crate::library::{CueRecord, KnownFile, RegisteredCueMetadata, ScanResult, ScanUncertainty};
use crate::library::{ImportBudget, ImportBudgetScope};
use anyhow::{Context, Result, bail};
use rivu_audio::MediaInfo;
use rivu_protocol::model::CueSegment;
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Bump when probing semantics change; fingerprints remain reusable.
pub(super) const PROBE_VERSION: i64 = 1;

/// Semantic facts and admission counts from one committed scan transaction.
///
/// Callers publish each affected projection once, rather than treating `changed`
/// as a catalog reload request. Duplicate members also depend on `library` and
/// `duplicate_groups`; `duplicate_members` covers their extra verification facts.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ScanImpacts {
    /// Any persisted track write, including hidden raw metadata and probe state.
    pub changed: bool,
    /// Effective full Track values (raw metadata hidden by overrides is excluded).
    pub library: bool,
    /// Effective queue-row values changed for an already queued identity.
    pub queue: bool,
    /// Album membership, ordering, member rows, or representative values changed.
    pub albums: bool,
    /// Track insertion or a changed path/CUE identity; missing is not removal.
    pub structure: bool,
    /// Ordinary fingerprint membership or present/missing grouping facts.
    pub duplicate_groups: bool,
    /// Size/mtime verification facts for fingerprinted ordinary members.
    pub duplicate_members: bool,
    /// Successfully scanned new identities excluded by the duration threshold.
    pub skipped_short_tracks: usize,
}
/// Derived invalidation and projection responsibilities for one committed scan.
/// Keep these rules beside the producer facts so callers cannot invent divergent
/// `library || queue` or cache-domain combinations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanImpactDependencies {
    pub library_revision: bool,
    pub structure_revision: bool,
    pub queue_revision: bool,
    pub album_revision: bool,
    pub duplicate_group_revision: bool,
    pub duplicate_member_revision: bool,
    pub refresh_history: bool,
    pub refresh_track_total: bool,
    pub refresh_queue: bool,
    pub clear_track_cache: bool,
    pub clear_filtered_track_count: bool,
    pub clear_catalog_counts: bool,
    pub clear_queue_count: bool,
    pub clear_playlist_count: bool,
    pub clear_album_counts: bool,
    pub clear_duplicate_group_count: bool,
}

impl ScanImpacts {
    pub(crate) fn dependencies(self, playlist_created: bool) -> ScanImpactDependencies {
        ScanImpactDependencies {
            library_revision: self.library,
            structure_revision: self.structure,
            queue_revision: self.queue,
            album_revision: self.albums,
            duplicate_group_revision: self.duplicate_groups,
            duplicate_member_revision: self.duplicate_members,
            refresh_history: self.library,
            refresh_track_total: self.structure,
            refresh_queue: self.library || self.queue,
            clear_track_cache: self.library,
            clear_filtered_track_count: self.library,
            clear_catalog_counts: self.library || self.structure,
            clear_queue_count: self.library || self.queue,
            clear_playlist_count: self.library || playlist_created,
            clear_album_counts: self.albums,
            clear_duplicate_group_count: self.duplicate_groups,
        }
    }
}

/// Compare staged values before updating tracks; hidden raw changes are excluded.
pub(super) fn staged_projection_columns(cue: bool) -> String {
    let visible = "t.path IS NOT s.path
        OR COALESCE(t.title_override,t.raw_title) IS NOT COALESCE(t.title_override,s.raw_title)
        OR COALESCE(t.artist_override,t.raw_artist) IS NOT COALESCE(t.artist_override,s.raw_artist)
        OR COALESCE(t.album_override,t.raw_album) IS NOT COALESCE(t.album_override,s.raw_album)
        OR t.duration IS NOT s.duration OR t.codec IS NOT s.codec OR t.missing IS NOT 0
        OR t.bitrate_bps IS NOT s.bitrate_bps OR t.bitrate_exact IS NOT s.bitrate_exact
        OR t.channels IS NOT s.channels OR t.sample_rate IS NOT s.sample_rate
        OR t.bits_per_sample IS NOT s.bits_per_sample";
    let cue_values = if cue {
        "OR t.cue_sheet IS NOT s.cue_sheet OR t.cue_number IS NOT s.cue_number
         OR t.cue_start_frame IS NOT s.cue_start_frame OR t.cue_end_frame IS NOT s.cue_end_frame"
    } else {
        ""
    };
    let cue_number = if cue {
        "OR t.cue_number IS NOT s.cue_number"
    } else {
        ""
    };
    let full = format!(
        "{visible} OR t.fingerprint IS NOT s.fingerprint
        OR t.release_date IS NOT s.release_date {cue_values}"
    );
    let old_album = super::albums::eligible_member("t");
    let new_album = rivu_format::database::eligible_title("COALESCE(t.album_override,s.raw_album)");
    let representative = super::albums::representative("t");
    format!(
        "COALESCE(MAX(({visible} {cue_number}) AND EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=0 AND track_id=t.id)),0),
        COALESCE(MAX(((t.id IS NULL OR {visible} {cue_number}
            OR t.track_number IS NOT s.track_number OR t.disc_number IS NOT s.disc_number)
            AND ({old_album} OR {new_album})) OR (({full}) AND {representative})),0)"
    )
}

// Keep equality and descendant branches separate so both path indexes can seek.
// A slash-terminated prefix has the exclusive BINARY upper bound ending in '0'.
const SCAN_SCOPE_IDS: &str = "
    SELECT t.id FROM json_each(?1) AS scope
    CROSS JOIN tracks AS t INDEXED BY tracks_path WHERE t.path=scope.value
    UNION
    SELECT t.id FROM json_each(?2) AS scope
    CROSS JOIN tracks AS t INDEXED BY tracks_path
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

fn scan_scope<'a>(
    paths: &'a [PathBuf],
    descendants: bool,
    mut retained: Option<&mut ImportBudgetScope>,
) -> Result<(HashSet<&'a str>, HashSet<String>)> {
    if let Some(retained) = &mut retained {
        // Conservative allowance for both hash indexes, independent of successful
        // track count. Variable-length prefix payloads are charged separately.
        retained.reserve(
            paths
                .len()
                .checked_mul(256)
                .context("Scan scope size overflow")?,
        )?;
    }
    let mut exact = HashSet::with_capacity(paths.len());
    let mut prefixes = HashSet::with_capacity(if descendants { paths.len() } else { 0 });
    for path in paths {
        let normalized = crate::library::logical_path(path)?;
        if normalized != *path {
            bail!(
                "Scan scope is not a normalized absolute path: {}",
                path.display()
            );
        }
        let path = path.to_str().context("Scan scope path is not UTF-8")?;
        if exact.insert(path) && descendants {
            let path = path.trim_end_matches('/');
            let bytes = path
                .len()
                .checked_add(1)
                .context("Scan scope size overflow")?;
            if let Some(retained) = &mut retained {
                retained.reserve(bytes)?;
            }
            let mut prefix = String::with_capacity(bytes);
            prefix.push_str(path);
            prefix.push('/');
            prefixes.insert(prefix);
        }
    }
    Ok((exact, prefixes))
}

/// JSON buffers own their reservation until SQLite has finished reading them.
/// Charge vector growth before appending, including JSON escaping and spare capacity.
struct ScopeJson {
    bytes: Vec<u8>,
    retained: Option<ImportBudgetScope>,
}

impl ScopeJson {
    fn serialize(value: &impl serde::Serialize, budget: Option<&ImportBudget>) -> Result<Self> {
        let mut output = Self {
            bytes: Vec::new(),
            retained: budget.map(ImportBudget::scoped),
        };
        serde_json::to_writer(&mut output, value)?;
        Ok(output)
    }
}

impl rusqlite::ToSql for ScopeJson {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(
            rusqlite::types::ValueRef::Text(&self.bytes),
        ))
    }
}

impl std::io::Write for ScopeJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(retained) = &mut self.retained {
            retained
                .reserve_vec(&mut self.bytes, bytes.len())
                .map_err(std::io::Error::other)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Existing {
    id: i64,
    path: PathBuf,
    hash: Option<String>,
    cue: Option<CueSegment>,
    independent: bool,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum TrackIdentity<'a> {
    File(&'a Path),
    Cue(&'a Path, u32),
}

fn identity<'a>(path: &'a Path, cue: Option<crate::library::CueView<'a>>) -> TrackIdentity<'a> {
    match cue {
        Some(cue) => TrackIdentity::Cue(cue.sheet, cue.number),
        None => TrackIdentity::File(path),
    }
}

#[derive(Hash, PartialEq, Eq)]
enum RelocationKey<'a> {
    File(&'a str),
    Cue(&'a str, u32, u64, Option<u64>),
}

fn relocation_key<'a>(
    hash: &'a str,
    cue: Option<crate::library::CueView<'_>>,
) -> RelocationKey<'a> {
    match cue {
        Some(cue) => RelocationKey::Cue(hash, cue.number, cue.start_frame, cue.end_frame),
        None => RelocationKey::File(hash),
    }
}

#[cfg(test)]
const KNOWN_COLUMNS: &str = "file_size,modified_ns,fingerprint,raw_title,raw_artist,raw_album,
    duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,
    cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,
    bits_per_sample,release_date,bitrate_exact,id,path";

fn known_file_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KnownFile> {
    Ok(KnownFile {
        size: r.get::<_, i64>(0)? as u64,
        modified_ns: r.get(1)?,
        fingerprint: fingerprint_from_row(r, 2)?,
        cue: cue_from_row(r, 10)?,
        media: if r.get::<_, i64>(14)? == PROBE_VERSION {
            Some(MediaInfo {
                title: r.get(3)?,
                artist: r.get(4)?,
                album: r.get(5)?,
                duration: r.get(6)?,
                codec: r.get(7)?,
                channels: r.get::<_, i64>(8)? as u16,
                sample_rate: r.get::<_, i64>(9)? as u32,
                bitrate_bps: r.get::<_, Option<i64>>(15)?.map(|value| value as u64),
                bitrate_exact: r.get(20)?,
                track_number: r.get(16)?,
                disc_number: r.get(17)?,
                bits_per_sample: r.get(18)?,
                release_date: r.get(19)?,
            })
        } else {
            None
        },
    })
}

/// Raw database identity and metadata used by integration test fixtures.
#[cfg(test)]
pub(crate) struct StoredScanFile {
    pub track_id: i64,
    pub path: PathBuf,
    pub size: u64,
    pub modified_ns: i64,
    pub fingerprint: Option<String>,
    pub media: Option<MediaInfo>,
    pub cue: Option<CueSegment>,
}

#[cfg(test)]
fn stored_scan_file_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredScanFile> {
    let KnownFile {
        size,
        modified_ns,
        fingerprint,
        media,
        cue,
    } = known_file_from_row(r)?;
    Ok(StoredScanFile {
        track_id: r.get(21)?,
        path: PathBuf::from(r.get::<_, String>(22)?),
        size,
        modified_ns,
        fingerprint,
        media,
        cue,
    })
}

impl Store {
    /// Read one reusable source snapshot, preferring an ordinary file identity.
    /// Both branches seek the path/identity index; no unrelated row is decoded.
    pub fn known_source(&self, path: &Path) -> Result<Option<KnownFile>> {
        let path = path.to_str().context("Scan source path is not UTF-8")?;
        let mut q = self.conn.prepare_cached(
            "SELECT file_size,modified_ns,fingerprint,raw_title,raw_artist,raw_album,
                    duration,codec,channels,sample_rate,cue_sheet,cue_number,cue_start_frame,
                    cue_end_frame,probe_version,bitrate_bps,track_number,disc_number,
                    bits_per_sample,release_date,bitrate_exact
             FROM tracks WHERE id=COALESCE(
                (SELECT id FROM tracks INDEXED BY tracks_path
                 WHERE path=?1 AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END = 0 ORDER BY id LIMIT 1),
                (SELECT id FROM tracks INDEXED BY tracks_path
                 WHERE path=?1 AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END IS NULL ORDER BY id LIMIT 1)
             )",
        )?;
        Ok(q.query_row([path], known_file_from_row).optional()?)
    }

    /// Inspect stored raw metadata for test fixtures, including every CUE identity.
    #[cfg(test)]
    pub(crate) fn stored_scan_files(&self, paths: &[PathBuf]) -> Result<Vec<StoredScanFile>> {
        let (exact, prefixes) = scan_scope(paths, true, None)?;
        let mut q = self.conn.prepare(&format!(
            "SELECT {KNOWN_COLUMNS} FROM tracks WHERE id IN ({SCAN_SCOPE_IDS}) ORDER BY id"
        ))?;
        Ok(q.query_map(
            params![
                serde_json::to_string(&exact)?,
                serde_json::to_string(&prefixes)?
            ],
            stored_scan_file_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    /// Refresh only identities still registered outside the current logical roots.
    /// CUE labels remain the registered identity's stored metadata; this operation
    /// never reparses sheets, discovers segments, relocates identities or admits rows.
    pub fn apply_other_tracks_check(
        &self,
        result: &mut ScanResult,
        library_roots: &[PathBuf],
    ) -> Result<ScanImpacts> {
        let current: HashSet<_> = self
            .other_tracks_sources(library_roots)?
            .into_iter()
            .collect();
        result.roots.retain(|path| current.contains(path));
        result
            .records
            .retain(|record| current.contains(&record.path));
        let mut records = Vec::new();
        result.segments.clear();
        let mut query = self.conn.prepare_cached(
            "SELECT cue_sheet,cue_number,cue_start_frame,cue_end_frame,
                    raw_title,raw_artist,raw_album,track_number,disc_number,release_date
             FROM tracks INDEXED BY tracks_path WHERE path=?1 ORDER BY id",
        )?;
        for source in result.records.drain(..) {
            let mut rows =
                query.query([source.path.to_str().context("Source path is not UTF-8")?])?;
            let mut ordinary = false;
            while let Some(row) = rows.next()? {
                let Some(cue) = cue_from_row(row, 0)? else {
                    ordinary = true;
                    continue;
                };
                let metadata = RegisteredCueMetadata {
                    title: row.get(4)?,
                    artist: row.get(5)?,
                    album: row.get(6)?,
                    track_number: row.get(7)?,
                    disc_number: row.get(8)?,
                    release_date: row.get(9)?,
                };
                let start = cue.start_frame as f64 / 75.0;
                let duration = source.media.duration.and_then(|duration| {
                    let end = cue.end_frame.map_or(duration, |frame| frame as f64 / 75.0);
                    (duration.is_finite() && start < duration && end > start)
                        .then_some(end.min(duration) - start)
                });
                result.segments.push(CueRecord::registered(
                    Arc::clone(&source),
                    cue,
                    metadata,
                    duration,
                ));
            }
            drop(rows);
            if ordinary {
                records.push(source);
            }
        }
        result.records = records;
        result.min_duration_secs = 0;
        self.apply_scan_scoped(result, true)
    }

    pub fn apply_scan(&self, result: &ScanResult) -> Result<ScanImpacts> {
        self.apply_scan_scoped(result, false)
    }

    fn apply_scan_scoped(&self, result: &ScanResult, exact_sources: bool) -> Result<ScanImpacts> {
        let staging = result
            .retained_budget
            .as_ref()
            .map(|budget| std::cell::RefCell::new(budget.scoped()));
        if let Some(staging) = &staging {
            // Borrowed effective rows plus bounded identity/hash-index storage.
            staging.borrow_mut().reserve(
                result.track_count() * (std::mem::size_of::<crate::library::ScanTrack<'_>>() + 256),
            )?;
        }
        let records: Vec<_> = result.tracks().collect();
        for r in &records {
            if r.source.path.to_str().is_none() {
                bail!("scan path is not UTF-8: {}", r.source.path.display());
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
        let transaction_started = std::time::Instant::now();
        let tx = self.conn.unchecked_transaction()?;
        let mut scope_workspace = result.retained_budget.as_ref().map(ImportBudget::scoped);
        let (mut exact, prefixes) =
            scan_scope(&result.roots, !exact_sources, scope_workspace.as_mut())?;
        exact.extend(records.iter().map(|record| {
            let owner = if exact_sources {
                record.source.path.as_path()
            } else {
                record
                    .cue
                    .as_ref()
                    .map_or(record.source.path.as_path(), |cue| cue.sheet.as_path())
            };
            owner.to_str().expect("scan record paths validated above")
        }));
        let exact_json = ScopeJson::serialize(&exact, result.retained_budget.as_ref())?;
        let prefixes_json = ScopeJson::serialize(&prefixes, result.retained_budget.as_ref())?;
        let mut selected = HashMap::new();
        let read_existing = |r: &rusqlite::Row<'_>| {
            if let Some(staging) = &staging {
                let text_bytes = |column| -> rusqlite::Result<usize> {
                    Ok(match r.get_ref(column)? {
                        rusqlite::types::ValueRef::Text(bytes) => bytes.len(),
                        _ => 0,
                    })
                };
                staging
                    .borrow_mut()
                    .reserve(
                        3 * std::mem::size_of::<Existing>() + 128 + text_bytes(1)? + text_bytes(3)?,
                    )
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
            }
            Ok(Existing {
                id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                hash: fingerprint_from_row(r, 2)?,
                cue: cue_from_row(r, 3)?,
                independent: r.get(7)?,
            })
        };
        let scope_ids = if exact_sources {
            "SELECT t.id FROM json_each(?1) AS scope
             CROSS JOIN tracks AS t INDEXED BY tracks_path WHERE t.path=scope.value
             UNION SELECT t.id FROM json_each(?2) AS scope
             CROSS JOIN tracks AS t INDEXED BY tracks_path WHERE t.path=scope.value"
        } else {
            SCAN_SCOPE_IDS
        };
        let mut q = tx.prepare(&format!(
            "SELECT id,path,fingerprint,cue_sheet,cue_number,cue_start_frame,cue_end_frame,relocation_independent
             FROM tracks WHERE id IN ({scope_ids})"
        ))?;
        for row in q.query_map(params![exact_json, prefixes_json], read_existing)? {
            let row = row?;
            selected.insert(row.id, row);
        }
        drop(q);
        drop((exact_json, prefixes_json, exact, prefixes, scope_workspace));
        // Exact identities never relocate. Only genuinely new destinations need
        // global same-fingerprint rows, even on unchanged duplicate-heavy scans.
        let existing_identities: HashSet<_> = selected
            .values()
            .map(|record| identity(&record.path, record.cue.as_ref().map(Into::into)))
            .collect();
        let unresolved = || {
            records.iter().filter(|record| {
                !existing_identities.contains(&identity(&record.source.path, record.cue))
            })
        };
        let candidate_hashes: HashSet<&str> = if exact_sources || result.scan_incomplete {
            HashSet::new()
        } else {
            unresolved()
                .filter_map(|record| record.source.fingerprint.as_deref())
                .collect()
        };
        let ordinary_candidate_hashes: HashSet<&str> = if exact_sources || result.scan_incomplete {
            HashSet::new()
        } else {
            unresolved()
                .filter(|record| record.cue.is_none())
                .filter_map(|record| record.source.fingerprint.as_deref())
                .collect()
        };
        drop(existing_identities);
        // Incomplete scans cannot relocate or mark missing. Exact record identities
        // are already in scope, so global same-content rows cannot affect this commit.
        if !exact_sources && !result.scan_incomplete {
            let mut q = tx.prepare(
                "SELECT id,path,fingerprint,cue_sheet,cue_number,cue_start_frame,cue_end_frame,relocation_independent
                 FROM tracks INDEXED BY tracks_fingerprint WHERE fingerprint=?1",
            )?;
            for hash in &candidate_hashes {
                let hash =
                    blake3::Hash::from_hex(hash).context("Invalid scan BLAKE3 fingerprint")?;
                let mut rows = q.query([hash.as_bytes().as_slice()])?;
                while let Some(row) = rows.next()? {
                    let id = row.get::<_, i64>(0)?;
                    if let std::collections::hash_map::Entry::Vacant(entry) = selected.entry(id) {
                        entry.insert(read_existing(row)?);
                    }
                }
            }
        }
        // The original table scan visited integer primary keys in row order.
        // Preserve that order for identities and global relocation candidates.
        let mut old: Vec<Existing> = selected.into_values().collect();
        old.sort_unstable_by_key(|record| record.id);
        let identities: HashSet<TrackIdentity<'_>> = records
            .iter()
            .map(|r| identity(&r.source.path, r.cue))
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
            if result.scan_incomplete && !exact_sources {
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
            let key = identity(&record.path, record.cue.as_ref().map(Into::into));
            let owner = if exact_sources {
                record.path.as_path()
            } else {
                record
                    .cue
                    .as_ref()
                    .map_or(record.path.as_path(), |cue| cue.sheet.as_path())
            };
            if !exact_sources
                && !identities.contains(&key)
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
                        .entry(relocation_key(hash, record.cue.as_ref().map(Into::into)))
                        .and_modify(|candidate| *candidate = None)
                        .or_insert(Some(record));
                }
            }
            byidentity.insert(key, record.id);
        }
        // Moves require uniqueness on both sides. Resolve exact identities first;
        // never let scan order choose one of several newly found copies.
        let mut new_destinations: HashMap<RelocationKey<'_>, Option<TrackIdentity<'_>>> =
            HashMap::new();
        for record in &records {
            let destination = identity(&record.source.path, record.cue);
            if !byidentity.contains_key(&destination)
                && let Some(hash) = record.source.fingerprint.as_deref()
            {
                new_destinations
                    .entry(relocation_key(hash, record.cue))
                    .and_modify(|candidate| {
                        if candidate.as_ref() != Some(&destination) {
                            *candidate = None;
                        }
                    })
                    .or_insert(Some(destination));
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
                bitrate_exact INTEGER NOT NULL,
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
                cue_end_frame,probe_version,bitrate_bps,bitrate_exact,track_number,disc_number,bits_per_sample,
                release_date
            ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )?;
        let mut skipped_short_tracks = 0;
        for r in &records {
            let key = identity(&r.source.path, r.cue);
            let id = byidentity.get(&key).copied().or_else(|| {
                r.source.fingerprint.as_deref().and_then(|hash| {
                    let relocation = relocation_key(hash, r.cue);
                    if !new_destinations
                        .get(&relocation)
                        .is_some_and(Option::is_some)
                    {
                        return None;
                    }
                    let candidate = candidates_by_key.get(&relocation).copied().flatten()?;
                    (!seen.contains(&candidate.id)).then_some(candidate.id)
                })
            });
            if exact_sources && id.is_none() {
                continue;
            }
            // Resolve exact identities and the existing relocation contract before
            // applying admission. Short existing tracks still refresh their metadata.
            if id.is_none()
                && !r.duration_filter_exempt
                && result.excludes_new_track(r.media.duration)
            {
                skipped_short_tracks += 1;
                continue;
            }
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
                .source
                .bitrate_bps
                .map(i64::try_from)
                .transpose()
                .context("Audio bitrate exceeds database range")?;
            let fingerprint = r
                .source
                .fingerprint
                .as_deref()
                .map(blake3::Hash::from_hex)
                .transpose()
                .context("Invalid scan BLAKE3 fingerprint")?;
            stage.execute(params![
                id,
                r.source.path.to_str(),
                fingerprint.as_ref().map(|hash| hash.as_bytes().as_slice()),
                r.source.size as i64,
                r.source.modified_ns,
                m.title,
                m.artist,
                m.album,
                m.duration,
                m.source.codec,
                m.source.channels as i64,
                m.source.sample_rate as i64,
                sheet,
                number,
                start,
                end,
                PROBE_VERSION,
                bitrate,
                m.source.bitrate_exact && bitrate.is_some(),
                m.track_number,
                m.disc_number,
                m.source.bits_per_sample,
                m.release_date,
            ])?;
            // Missing detection visits only pre-scan IDs. New inserts cannot
            // belong to that set; resolved old IDs are already accounted for.
            if let Some(id) = id {
                seen.insert(id);
            }
        }
        drop(stage);
        // A queued scan may predate playback enrichment. Preserve precision only
        // for unchanged encoded content and the same CUE source restriction.
        tx.execute(
            "UPDATE temp.scan_stage AS s SET bitrate_bps=t.bitrate_bps,bitrate_exact=t.bitrate_exact
             FROM tracks AS t WHERE t.id=s.resolved_id AND s.bitrate_exact=0
             AND (s.bitrate_bps IS NULL OR t.bitrate_exact=1)
             AND t.bitrate_bps IS NOT NULL
             AND t.cue_sheet IS s.cue_sheet AND t.cue_number IS s.cue_number
             AND t.cue_start_frame IS s.cue_start_frame AND t.cue_end_frame IS s.cue_end_frame
             AND (
                (s.fingerprint IS NOT NULL AND t.fingerprint=s.fingerprint)
                OR (t.path=s.path AND t.file_size=s.file_size
                    AND t.modified_ns=s.modified_ns AND s.modified_ns<>?1
                    AND t.fingerprint IS s.fingerprint)
             )",
            [i64::MIN],
        )?;
        // Compare against the final staged values, including playback enrichment,
        // while the old rows and their overrides are still available. These flags
        // describe changed dependencies, not hashes of materialized query results.
        let mut impacts = tx.query_row(
            &format!("SELECT
                COALESCE(MAX(t.id IS NULL OR t.path IS NOT s.path
                    OR t.fingerprint IS NOT s.fingerprint
                    OR t.file_size IS NOT s.file_size
                    OR COALESCE(t.title_override,t.raw_title) IS NOT COALESCE(t.title_override,s.raw_title)
                    OR COALESCE(t.artist_override,t.raw_artist) IS NOT COALESCE(t.artist_override,s.raw_artist)
                    OR COALESCE(t.album_override,t.raw_album) IS NOT COALESCE(t.album_override,s.raw_album)
                    OR t.duration IS NOT s.duration OR t.codec IS NOT s.codec
                    OR t.channels IS NOT s.channels OR t.sample_rate IS NOT s.sample_rate
                    OR t.cue_sheet IS NOT s.cue_sheet OR t.cue_number IS NOT s.cue_number
                    OR t.cue_start_frame IS NOT s.cue_start_frame
                    OR t.cue_end_frame IS NOT s.cue_end_frame OR t.missing IS NOT 0
                    OR t.bitrate_bps IS NOT s.bitrate_bps OR t.bitrate_exact IS NOT s.bitrate_exact
                    OR t.track_number IS NOT s.track_number OR t.disc_number IS NOT s.disc_number
                    OR t.bits_per_sample IS NOT s.bits_per_sample
                    OR t.release_date IS NOT s.release_date),0),
                COALESCE(MAX(t.id IS NULL OR t.path IS NOT s.path
                    OR t.cue_sheet IS NOT s.cue_sheet OR t.cue_number IS NOT s.cue_number),0),
                COALESCE(MAX(
                    ((t.cue_sheet IS NULL AND t.fingerprint IS NOT NULL)
                        OR (s.cue_sheet IS NULL AND s.fingerprint IS NOT NULL))
                    AND (t.id IS NULL OR t.fingerprint IS NOT s.fingerprint
                        OR (t.cue_sheet IS NULL)<>(s.cue_sheet IS NULL)
                        OR t.missing IS NOT 0)),0),
                COALESCE(MAX(
                    ((t.cue_sheet IS NULL AND t.fingerprint IS NOT NULL)
                        OR (s.cue_sheet IS NULL AND s.fingerprint IS NOT NULL))
                    AND (t.id IS NULL OR t.file_size IS NOT s.file_size
                        OR t.modified_ns IS NOT s.modified_ns)),0),
                {}
             FROM temp.scan_stage AS s LEFT JOIN tracks AS t ON t.id=s.resolved_id", staged_projection_columns(true)),
            [],
            |row| {
                Ok(ScanImpacts {
                    library: row.get(0)?,
                    structure: row.get(1)?,
                    duplicate_groups: row.get(2)?,
                    duplicate_members: row.get(3)?,
                    queue: row.get(4)?,
                    albums: row.get(5)?,
                    skipped_short_tracks,
                    ..ScanImpacts::default()
                })
            },
        )?;
        let mut changed_tracks = tx.execute(
            "UPDATE tracks SET
                path=s.path,fingerprint=s.fingerprint,file_size=s.file_size,modified_ns=s.modified_ns,
                raw_title=s.raw_title,raw_artist=s.raw_artist,raw_album=s.raw_album,duration=s.duration,
                codec=s.codec,channels=s.channels,sample_rate=s.sample_rate,cue_sheet=s.cue_sheet,
                cue_number=s.cue_number,cue_start_frame=s.cue_start_frame,cue_end_frame=s.cue_end_frame,
                missing=0,probe_version=s.probe_version,bitrate_bps=s.bitrate_bps,bitrate_exact=s.bitrate_exact,
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
                OR tracks.bitrate_exact IS NOT s.bitrate_exact
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
                bitrate_bps,bitrate_exact,track_number,disc_number,bits_per_sample,release_date
             ) SELECT path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,duration,codec,
                channels,sample_rate,cue_sheet,cue_number,cue_start_frame,cue_end_frame,probe_version,
                bitrate_bps,bitrate_exact,track_number,disc_number,bits_per_sample,release_date
             FROM temp.scan_stage WHERE resolved_id IS NULL",
            [],
        )?;
        tx.execute_batch("DROP TABLE temp.scan_stage")?;
        let mut mark_missing = tx.prepare(&format!(
            "UPDATE tracks SET missing=1 WHERE id=?1 AND missing=0 RETURNING EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=0 AND track_id=tracks.id),{}",
            super::albums::eligible_member("tracks"),
        ))?;
        for o in &old {
            let owner = if exact_sources {
                o.path.as_path()
            } else {
                o.cue
                    .as_ref()
                    .map_or(o.path.as_path(), |cue| cue.sheet.as_path())
            };
            if !seen.contains(&o.id)
                && ((if exact_sources {
                    requested_roots.contains(owner)
                } else {
                    owner
                        .ancestors()
                        .any(|ancestor| requested_roots.contains(ancestor))
                }) || absent_ordinary.contains(&o.id))
                && !unconfirmed(o)
                && let Some((queue, albums)) = mark_missing
                    .query_row([o.id], |row| {
                        Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?))
                    })
                    .optional()?
            {
                changed_tracks += 1;
                impacts.library = true;
                impacts.queue |= queue;
                impacts.albums |= albums;
                impacts.duplicate_groups |= o.cue.is_none() && o.hash.is_some();
            }
        }
        drop(mark_missing);
        tx.commit()?;
        tracing::info!(
            elapsed_ms = transaction_started.elapsed().as_millis(),
            records = result.track_count(),
            "library_scan_transaction_committed"
        );
        impacts.changed = changed_tracks != 0;
        if impacts.dependencies(false).clear_track_cache {
            self.track_cache.borrow_mut().entries.clear();
        }
        Ok(impacts)
    }
}
