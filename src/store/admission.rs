use super::Store;
use super::positions::{PositionScope, insert_tracks};
use super::scan::{PROBE_VERSION, ScanImpacts};
use crate::library::ExternalAdmission;
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};

impl Store {
    /// Refresh exact ordinary identities and queue their requested occurrences in
    /// one transaction. No absence, relocation or duration policy is involved.
    pub(crate) fn admit_external_queue(
        &self,
        admission: &ExternalAdmission,
        index: Option<usize>,
        current_id: Option<u64>,
    ) -> Result<(Vec<u64>, ScanImpacts, usize)> {
        ensure!(
            admission
                .occurrences
                .iter()
                .all(|&index| index < admission.records.len()),
            "Invalid external source occurrence"
        );
        let tx = self.conn.unchecked_transaction()?;
        let total = usize::try_from(tx.query_row(
            "SELECT COALESCE(MAX(position)+1,0) FROM queue_entries",
            [],
            |row| row.get::<_, i64>(0),
        )?)
        .context("Invalid queue position")?;
        let insertion = index.unwrap_or(total);
        ensure!(insertion <= total, "Queue insertion index out of range");
        tx.execute_batch("CREATE TEMP TABLE external_sources(
            ordinal INTEGER PRIMARY KEY,resolved_id INTEGER,path TEXT NOT NULL,fingerprint BLOB,
            file_size INTEGER NOT NULL,modified_ns INTEGER NOT NULL,raw_title TEXT NOT NULL,
            raw_artist TEXT NOT NULL,raw_album TEXT NOT NULL,duration REAL,codec TEXT NOT NULL,
            channels INTEGER NOT NULL,sample_rate INTEGER NOT NULL,probe_version INTEGER NOT NULL,
            bitrate_bps INTEGER,bitrate_exact INTEGER NOT NULL,track_number INTEGER,disc_number INTEGER,
            bits_per_sample INTEGER,release_date TEXT);")?;
        {
            let mut existing = tx.prepare_cached("SELECT id FROM tracks INDEXED BY tracks_directory WHERE path=?1 AND cue_sheet IS NULL ORDER BY id LIMIT 1")?;
            let mut stage = tx.prepare("INSERT INTO external_sources VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)")?;
            for (ordinal, record) in admission.records.iter().enumerate() {
                ensure!(
                    record.cue.is_none(),
                    "External admission accepts only ordinary audio sources"
                );
                ensure!(
                    crate::library::logical_path(&record.path)? == record.path,
                    "External source path is not normalized"
                );
                let path = record
                    .path
                    .to_str()
                    .context("External source path is not UTF-8")?;
                let id = existing
                    .query_row([path], |row| row.get::<_, i64>(0))
                    .optional()?;
                let fingerprint = record
                    .fingerprint
                    .as_deref()
                    .map(blake3::Hash::from_hex)
                    .transpose()
                    .context("Invalid external audio BLAKE3 fingerprint")?;
                let media = &record.media;
                let bitrate = media
                    .bitrate_bps
                    .map(i64::try_from)
                    .transpose()
                    .context("Audio bitrate exceeds database range")?;
                stage.execute(params![
                    i64::try_from(ordinal)?,
                    id,
                    path,
                    fingerprint.as_ref().map(|hash| hash.as_bytes().as_slice()),
                    i64::try_from(record.size).context("Audio file size exceeds database range")?,
                    record.modified_ns,
                    media.title,
                    media.artist,
                    media.album,
                    media.duration,
                    media.codec,
                    i64::from(media.channels),
                    i64::from(media.sample_rate),
                    PROBE_VERSION,
                    bitrate,
                    media.bitrate_exact && bitrate.is_some(),
                    media.track_number,
                    media.disc_number,
                    media.bits_per_sample,
                    media.release_date,
                ])?;
            }
        }
        // Preserve playback's more precise bitrate only for unchanged content,
        // exactly as ordinary scans do; fresh encoded content replaces old facts.
        tx.execute("UPDATE external_sources AS s SET bitrate_bps=t.bitrate_bps,bitrate_exact=t.bitrate_exact
            FROM tracks AS t WHERE t.id=s.resolved_id AND s.bitrate_exact=0
            AND (s.bitrate_bps IS NULL OR t.bitrate_exact=1) AND t.bitrate_bps IS NOT NULL
            AND ((s.fingerprint IS NOT NULL AND t.fingerprint=s.fingerprint)
                OR (t.path=s.path AND t.file_size=s.file_size AND t.modified_ns=s.modified_ns
                    AND s.modified_ns<>?1 AND t.fingerprint IS s.fingerprint))", [i64::MIN])?;
        let mut impacts = tx.query_row(&format!("SELECT
            COALESCE(MAX(t.id IS NULL OR t.fingerprint IS NOT s.fingerprint
                OR COALESCE(t.title_override,t.raw_title) IS NOT COALESCE(t.title_override,s.raw_title)
                OR COALESCE(t.artist_override,t.raw_artist) IS NOT COALESCE(t.artist_override,s.raw_artist)
                OR COALESCE(t.album_override,t.raw_album) IS NOT COALESCE(t.album_override,s.raw_album)
                OR t.duration IS NOT s.duration OR t.codec IS NOT s.codec OR t.channels IS NOT s.channels
                OR t.sample_rate IS NOT s.sample_rate OR t.missing IS NOT 0
                OR t.bitrate_bps IS NOT s.bitrate_bps OR t.bitrate_exact IS NOT s.bitrate_exact
                OR t.track_number IS NOT s.track_number OR t.disc_number IS NOT s.disc_number
                OR t.bits_per_sample IS NOT s.bits_per_sample OR t.release_date IS NOT s.release_date),0),
            COALESCE(MAX(t.id IS NULL),0),
            COALESCE(MAX((t.fingerprint IS NOT NULL OR s.fingerprint IS NOT NULL)
                AND (t.id IS NULL OR t.fingerprint IS NOT s.fingerprint OR t.missing IS NOT 0)),0),
            COALESCE(MAX((t.fingerprint IS NOT NULL OR s.fingerprint IS NOT NULL)
                AND (t.id IS NULL OR t.file_size IS NOT s.file_size OR t.modified_ns IS NOT s.modified_ns)),0),
            {}
            FROM external_sources AS s LEFT JOIN tracks AS t ON t.id=s.resolved_id", super::scan::staged_projection_columns(false)), [], |row| {
            Ok(ScanImpacts { library: row.get(0)?, structure: row.get(1)?, duplicate_groups: row.get(2)?, duplicate_members: row.get(3)?, queue: row.get(4)?, albums: row.get(5)?, ..ScanImpacts::default() })
        })?;
        let changed = tx.execute("UPDATE tracks SET
            fingerprint=s.fingerprint,file_size=s.file_size,modified_ns=s.modified_ns,
            raw_title=s.raw_title,raw_artist=s.raw_artist,raw_album=s.raw_album,duration=s.duration,
            codec=s.codec,channels=s.channels,sample_rate=s.sample_rate,missing=0,probe_version=s.probe_version,
            bitrate_bps=s.bitrate_bps,bitrate_exact=s.bitrate_exact,track_number=s.track_number,
            disc_number=s.disc_number,bits_per_sample=s.bits_per_sample,release_date=s.release_date
            FROM external_sources AS s WHERE tracks.id=s.resolved_id AND (
                tracks.fingerprint IS NOT s.fingerprint OR tracks.file_size IS NOT s.file_size
                OR tracks.modified_ns IS NOT s.modified_ns OR tracks.raw_title IS NOT s.raw_title
                OR tracks.raw_artist IS NOT s.raw_artist OR tracks.raw_album IS NOT s.raw_album
                OR tracks.duration IS NOT s.duration OR tracks.codec IS NOT s.codec
                OR tracks.channels IS NOT s.channels OR tracks.sample_rate IS NOT s.sample_rate
                OR tracks.missing IS NOT 0 OR tracks.probe_version IS NOT s.probe_version
                OR tracks.bitrate_bps IS NOT s.bitrate_bps OR tracks.bitrate_exact IS NOT s.bitrate_exact
                OR tracks.track_number IS NOT s.track_number OR tracks.disc_number IS NOT s.disc_number
                OR tracks.bits_per_sample IS NOT s.bits_per_sample OR tracks.release_date IS NOT s.release_date)", [])?;
        let added = tx.execute("INSERT INTO tracks(path,fingerprint,file_size,modified_ns,
            raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate,probe_version,
            bitrate_bps,bitrate_exact,track_number,disc_number,bits_per_sample,release_date)
            SELECT path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,duration,
            codec,channels,sample_rate,probe_version,bitrate_bps,bitrate_exact,track_number,
            disc_number,bits_per_sample,release_date FROM external_sources WHERE resolved_id IS NULL ORDER BY ordinal", [])?;
        tx.execute("UPDATE external_sources AS s SET resolved_id=(SELECT id FROM tracks INDEXED BY tracks_directory WHERE path=s.path AND cue_sheet IS NULL ORDER BY id LIMIT 1) WHERE resolved_id IS NULL", [])?;
        let resolved = tx
            .prepare("SELECT resolved_id FROM external_sources ORDER BY ordinal")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let ids: Vec<_> = admission
            .occurrences
            .iter()
            .map(|&index| resolved[index])
            .collect();
        let count = insert_tracks(
            &tx,
            PositionScope::Queue,
            insertion,
            &serde_json::to_string(&ids)?,
            None,
        )?;
        let end = insertion
            .checked_add(count)
            .context("Queue insertion overflow")?;
        let inserted = tx.prepare("SELECT id FROM queue_entries WHERE position>=?1 AND position<?2 ORDER BY position,id")?
            .query_map(params![i64::try_from(insertion)?, i64::try_from(end)?], |row| {
                let value = row.get::<_, i64>(0)?;
                u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
            })?
            .collect::<rusqlite::Result<Vec<u64>>>()?;
        let current_id = current_id
            .map(i64::try_from)
            .transpose()
            .context("Queue entry ID exceeds database range")?;
        tx.execute(
            "UPDATE rivu_meta SET current_queue_entry_id=?1 WHERE singleton=1 AND current_queue_entry_id IS NOT ?1",
            [current_id],
        )?;
        tx.execute_batch("DROP TABLE external_sources;")?;
        tx.commit()?;
        impacts.changed = changed != 0 || added != 0;
        impacts.queue |= !inserted.is_empty();
        if impacts.changed {
            self.invalidate_caches();
        }
        Ok((inserted, impacts, added))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{ScanRecord, ScanResult};
    use rivu_audio::MediaInfo;
    use rivu_protocol::model::CueSegment;
    use std::path::Path;

    fn record(path: &str) -> ScanRecord {
        ScanRecord {
            path: path.into(),
            size: 1,
            modified_ns: 2,
            fingerprint: Some(blake3::hash(path.as_bytes()).to_hex().to_string()),
            cue: None,
            media: MediaInfo {
                title: path.into(),
                artist: String::new(),
                album: String::new(),
                duration: Some(0.01),
                codec: "pcm".into(),
                channels: 1,
                sample_rate: 8_000,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: None,
                disc_number: None,
                bits_per_sample: Some(16),
                release_date: None,
            },
        }
    }

    fn admission(paths: &[&str], occurrences: Vec<usize>) -> ExternalAdmission {
        ExternalAdmission {
            records: paths.iter().map(|path| record(path)).collect(),
            occurrences,
            rejected: 0,
            errors: Vec::new(),
        }
    }

    #[test]
    fn admission_persists_short_sources_and_duplicate_occurrences_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("library.db");
        let store = Store::open(&database).unwrap();
        let prepared = admission(
            &["/outside/second.wav", "/outside/first.wav"],
            vec![0, 1, 0],
        );
        let (inserted, impacts, added) = store.admit_external_queue(&prepared, None, None).unwrap();
        assert_eq!(inserted.len(), 3);
        assert_eq!(added, 2);
        assert!(impacts.structure);
        assert!(store.queue_state().unwrap().current_id.is_none());
        let expected: Vec<_> = store
            .queue_page(None, 0, 10)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.track_id)
            .collect();
        assert_eq!(expected[0], expected[2]);
        assert_ne!(expected[0], expected[1]);
        drop(store);
        let store = Store::open(&database).unwrap();
        assert_eq!(
            store
                .queue_page(None, 0, 10)
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row.track_id)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(store.track_count().unwrap(), 2);
        assert_eq!(
            store.track(expected[0]).unwrap().unwrap().duration,
            Some(0.01)
        );
    }

    #[test]
    fn invalid_position_and_failed_queue_write_leave_no_registered_residue() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let prepared = admission(&["/outside/one.wav"], vec![0]);
        store.admit_external_queue(&prepared, None, None).unwrap();
        let next = admission(&["/outside/two.wav"], vec![0]);
        assert!(store.admit_external_queue(&next, Some(2), None).is_err());
        assert!(
            store
                .track_id_for_source(Path::new("/outside/two.wav"), None)
                .unwrap()
                .is_none()
        );
        let before = store.queue_state().unwrap();
        store.conn.execute_batch("CREATE TEMP TRIGGER reject_admission BEFORE INSERT ON queue_entries BEGIN SELECT RAISE(ABORT,'rejected queue write'); END;").unwrap();
        assert!(store.admit_external_queue(&next, None, None).is_err());
        assert!(
            store
                .track_id_for_source(Path::new("/outside/two.wav"), None)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.track_count().unwrap(), 1);
        assert_eq!(store.queue_state().unwrap(), before);
    }

    #[test]
    fn positional_admission_preserves_current_occurrence() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let existing = admission(&["/outside/old.wav"], vec![0, 0]);
        store.admit_external_queue(&existing, None, None).unwrap();
        let old = store.queue_ids().unwrap();
        store.set_queue_current(Some(old[1])).unwrap();
        let prepared = admission(&["/outside/new.wav"], vec![0, 0]);
        let before = store.queue_state().unwrap();
        let (inserted, _, _) = store
            .admit_external_queue(&prepared, Some(1), Some(old[1]))
            .unwrap();
        assert_eq!(
            store.queue_ids().unwrap(),
            vec![old[0], inserted[0], inserted[1], old[1]]
        );
        let after = store.queue_state().unwrap();
        assert_eq!(after.current_id, before.current_id);
        assert_eq!(after.current_position, Some(3));
    }

    #[test]
    fn ordinary_identity_is_reused_without_cue_reconciliation_or_relocation() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut cue = record("/outside/shared.wav");
        cue.cue = Some(CueSegment {
            sheet: "/outside/shared.cue".into(),
            number: 1,
            start_frame: 0,
            end_frame: None,
        });
        let ordinary = record("/outside/existing.wav");
        let absent = record("/outside/absent.wav");
        store
            .apply_scan(&ScanResult {
                records: vec![cue, ordinary, absent],
                ..ScanResult::default()
            })
            .unwrap();
        let ordinary_id = store
            .track_id_for_source(Path::new("/outside/existing.wav"), None)
            .unwrap()
            .unwrap();
        store.conn.execute("UPDATE tracks SET title_override='Keep title',favorite=1,play_count=7,missing=1 WHERE id=?1", [ordinary_id]).unwrap();
        let mut prepared = admission(
            &[
                "/outside/shared.wav",
                "/outside/existing.wav",
                "/outside/copied.wav",
            ],
            vec![1, 0, 1, 2],
        );
        prepared.records[2].fingerprint = record("/outside/absent.wav").fingerprint;
        let (_, _, added) = store.admit_external_queue(&prepared, None, None).unwrap();
        assert_eq!(added, 2);
        let track = store.track(ordinary_id).unwrap().unwrap();
        assert_eq!(track.title, "Keep title");
        assert!(track.favorite);
        assert_eq!(track.play_count, 7);
        assert!(!track.missing);
        assert!(
            store
                .track_id_for_source(Path::new("/outside/shared.cue"), Some(1))
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .track_id_for_source(Path::new("/outside/shared.wav"), None)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .track_id_for_source(Path::new("/outside/absent.wav"), None)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .queue_page(None, 0, 10)
                .unwrap()
                .rows
                .into_iter()
                .filter(|row| row.track_id == ordinary_id)
                .count(),
            2
        );
    }

    #[test]
    fn existing_raw_and_technical_facts_refresh_without_losing_user_state_or_references() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut prepared = admission(&["/outside/existing.wav"], vec![0]);
        store.admit_external_queue(&prepared, None, None).unwrap();
        let old_queue = store.queue_ids().unwrap();
        let id = store.queue_track_id(old_queue[0]).unwrap();
        let playlist = store
            .create_playlist_with_tracks("Retained", &[id])
            .unwrap();
        store.conn.execute("UPDATE tracks SET title_override='User title',artist_override='User artist',album_override='User album',favorite=1,play_count=7,last_played=42,missing=1 WHERE id=?1", [id]).unwrap();
        let before = store.track(id).unwrap().unwrap();
        prepared.records[0].size = 1234;
        prepared.records[0].modified_ns = 5678;
        prepared.records[0].fingerprint =
            Some(blake3::hash(b"fresh encoded content").to_hex().to_string());
        let media = &mut prepared.records[0].media;
        media.title = "Fresh raw title".into();
        media.artist = "Fresh raw artist".into();
        media.album = "Fresh raw album".into();
        media.duration = Some(2.5);
        media.codec = "flac".into();
        media.sample_rate = 96_000;
        media.channels = 2;
        media.bitrate_bps = Some(512_000);
        media.bitrate_exact = true;
        prepared.occurrences = vec![0, 0];
        let (inserted, impacts, added) = store
            .admit_external_queue(&prepared, None, Some(old_queue[0]))
            .unwrap();
        assert_eq!(added, 0);
        assert!(
            impacts.changed
                && impacts.library
                && impacts.duplicate_groups
                && impacts.duplicate_members
        );
        assert!(!impacts.structure);
        let refreshed = store.track(id).unwrap().unwrap();
        assert_eq!(refreshed.id, id);
        assert_eq!(refreshed.title, before.title);
        assert_eq!(refreshed.artist, before.artist);
        assert_eq!(refreshed.album, before.album);
        assert_eq!(refreshed.favorite, before.favorite);
        assert_eq!(refreshed.play_count, before.play_count);
        assert_eq!(refreshed.last_played, before.last_played);
        assert_eq!(refreshed.duration, Some(2.5));
        assert_eq!(refreshed.codec, "flac");
        assert_eq!(refreshed.sample_rate, 96_000);
        assert!(!refreshed.missing);
        let known = store
            .known_source(Path::new("/outside/existing.wav"))
            .unwrap()
            .unwrap();
        assert_eq!(known.fingerprint, prepared.records[0].fingerprint);
        assert_eq!(known.size, 1234);
        assert_eq!(known.modified_ns, 5678);
        assert_eq!(known.media.unwrap().title, "Fresh raw title");
        assert_eq!(
            store.queue_ids().unwrap(),
            vec![old_queue[0], inserted[0], inserted[1]]
        );
        assert_eq!(store.queue_track_id(inserted[0]).unwrap(), id);
        assert_eq!(store.queue_track_id(inserted[1]).unwrap(), id);
        assert_eq!(
            store
                .playlist_entries_page(playlist, None, 0, 10)
                .unwrap()
                .rows[0]
                .track_id,
            id
        );
    }
}
