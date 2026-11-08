use super::positions::insert_staged_tracks;
use super::scan::{
    PROBE_VERSION, effective_source_from_row, effective_source_select, staged_bitrate_update_sql,
    staged_insert_sql, staged_library_column, staged_table_schema, staged_update_sql,
};
use super::{CatalogChanges, Store};
use crate::library::ExternalAdmission;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};

impl Store {
    /// Refresh exact ordinary identities and queue their requested occurrences in
    /// one transaction. No absence, relocation or duration policy is involved.
    pub(crate) fn admit_external_queue(
        &self,
        admission: &ExternalAdmission,
        index: Option<usize>,
        current_id: Option<u64>,
    ) -> Result<(usize, CatalogChanges, usize)> {
        let tx = self.conn.unchecked_transaction()?;
        let current_id = current_id
            .map(i64::try_from)
            .transpose()
            .context("Queue entry ID exceeds database range")?;
        let observed = || {
            tx.query_row(
                effective_source_select!(
                    "(SELECT track_id FROM playlist_entries WHERE playlist_id=0 AND id=?1)"
                ),
                [current_id],
                effective_source_from_row,
            )
            .optional()
        };
        let current_before = observed()?;
        tx.execute_batch(&format!(
            "CREATE TEMP TABLE external_sources({})",
            staged_table_schema("ordinal INTEGER PRIMARY KEY,resolved_id INTEGER", false),
        ))?;
        {
            let mut existing = tx.prepare_cached("SELECT id FROM tracks INDEXED BY tracks_path WHERE path=?1 AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END = 0 ORDER BY id LIMIT 1")?;
            let mut stage = tx.prepare("INSERT INTO external_sources VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)")?;
            for (ordinal, record) in admission.records().iter().enumerate() {
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
        tx.execute(
            &staged_bitrate_update_sql("external_sources", false),
            [i64::MIN],
        )?;
        let mut impacts = tx.query_row(&format!("SELECT
            {},
            COALESCE(MAX(t.id IS NULL),0),
            COALESCE(MAX((t.fingerprint IS NOT NULL OR s.fingerprint IS NOT NULL)
                AND (t.id IS NULL OR t.fingerprint IS NOT s.fingerprint OR t.missing IS NOT 0)),0),
            COALESCE(MAX((t.fingerprint IS NOT NULL OR s.fingerprint IS NOT NULL)
                AND (t.id IS NULL OR t.file_size IS NOT s.file_size OR t.modified_ns IS NOT s.modified_ns)),0),
            {}
            FROM external_sources AS s LEFT JOIN tracks AS t ON t.id=s.resolved_id", staged_library_column(false), super::scan::staged_projection_columns(false)), [], |row| {
            Ok(CatalogChanges { library: row.get(0)?, structure: row.get(1)?, duplicate_groups: row.get(2)?, duplicate_members: row.get(3)?, queue: row.get(4)?, albums: row.get(5)?, history: row.get(6)?, ..CatalogChanges::default() })
        })?;
        let changed = tx.execute(&staged_update_sql("external_sources", false), [])?;
        let added = tx.execute(
            &staged_insert_sql("external_sources", false, "ORDER BY ordinal"),
            [],
        )?;
        tx.execute("UPDATE external_sources AS s SET resolved_id=(SELECT id FROM tracks INDEXED BY tracks_path WHERE path=s.path AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END = 0 ORDER BY id LIMIT 1) WHERE resolved_id IS NULL", [])?;
        tx.execute_batch("CREATE TEMP TABLE insertion_tracks(ordinal INTEGER PRIMARY KEY,track_id INTEGER NOT NULL);")?;
        {
            let mut stage = tx.prepare(
                "INSERT INTO insertion_tracks(ordinal,track_id) VALUES(?1,(SELECT resolved_id FROM external_sources WHERE ordinal=?2))",
            )?;
            for (ordinal, &source_index) in admission.occurrences().iter().enumerate() {
                stage.execute(params![
                    i64::try_from(ordinal)?,
                    i64::try_from(source_index)?,
                ])?;
            }
        }
        let count = insert_staged_tracks(&tx, 0, index)?;
        tx.execute(
            "UPDATE rivu_meta SET current_queue_entry_id=?1 WHERE singleton=1 AND current_queue_entry_id IS NOT ?1",
            [current_id],
        )?;
        tx.execute_batch("DROP TABLE external_sources;")?;
        impacts.current_track = Some(observed()? != current_before);
        tx.commit()?;
        impacts.changed = changed != 0 || added != 0;
        impacts.queue |= count != 0;
        if impacts.changed {
            self.invalidate_caches();
        }
        Ok((count, impacts, added))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{CueRecord, RegisteredCueMetadata, ScanRecord, ScanResult};
    use rivu_audio::MediaInfo;
    use rivu_protocol::model::CueSegment;
    use std::path::Path;

    fn record(path: &str) -> ScanRecord {
        ScanRecord {
            path: path.into(),
            size: 1,
            modified_ns: 2,
            fingerprint: Some(blake3::hash(path.as_bytes()).to_hex().to_string()),
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
        ExternalAdmission::from_test_records(
            paths.iter().map(|path| record(path)).collect(),
            occurrences,
        )
    }

    #[test]
    fn source_observation_tolerates_malformed_fields_and_compares_effective_changes() {
        for scan in [false, true] {
            let store = Store::open(Path::new(":memory:")).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO tracks(id,path) VALUES(100,'/unrelated.wav')",
                    [],
                )
                .unwrap();
            store
                .admit_external_queue(&admission(&["/current.wav"], vec![0]), None, None)
                .unwrap();
            let current_entry = store.queue_ids().unwrap()[0];
            let current_track = store.queue_track_id(current_entry).unwrap();
            assert_ne!(current_track, current_entry as i64);
            store.conn.execute(
                "UPDATE tracks SET play_count='malformed',duration='malformed',title_override='User title' WHERE id=?1",
                [current_track],
            ).unwrap();
            let apply = |source: ScanRecord| {
                if scan {
                    store
                        .apply_scan_observing(
                            &ScanResult {
                                roots: vec![source.path.clone()],
                                records: vec![std::sync::Arc::new(source)],
                                min_duration_secs: 0,
                                ..Default::default()
                            },
                            Some(current_track),
                        )
                        .unwrap()
                } else {
                    store
                        .admit_external_queue(
                            &ExternalAdmission::from_test_records(vec![source], vec![0]),
                            None,
                            Some(current_entry),
                        )
                        .unwrap()
                        .1
                }
            };
            let unrelated = apply(record("/new.wav"));
            assert!(unrelated.changed);
            assert_eq!(unrelated.current_track, Some(false));
            assert!(store.known_source(Path::new("/new.wav")).unwrap().is_some());
            store
                .conn
                .execute(
                    "UPDATE tracks SET duration=?1 WHERE id=?2",
                    params![record("/current.wav").media.duration, current_track],
                )
                .unwrap();
            let mut source = record("/current.wav");
            source.media.title = "New hidden raw title".into();
            let hidden = apply(source.clone());
            assert!(hidden.changed);
            assert_eq!(hidden.current_track, Some(false));
            source.media.codec = "flac".into();
            let visible = apply(source);
            assert_eq!(visible.current_track, Some(true));
            assert_eq!(
                store.conn.query_row(
                    "SELECT COALESCE(title_override,raw_title),codec,play_count FROM tracks WHERE id=?1",
                    [current_track],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
                ).unwrap(),
                ("User title".into(), "flac".into(), "malformed".into()),
            );
        }
    }

    #[test]
    fn external_admission_size_only_changes_invalidate_library_stats() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut prepared = admission(&["/outside/existing.wav"], vec![0]);
        store.admit_external_queue(&prepared, None, None).unwrap();
        let mut updated = record("/outside/existing.wav");
        updated.size = 1234;
        prepared = ExternalAdmission::from_test_records(vec![updated], vec![0]);
        let (_, impacts, added) = store.admit_external_queue(&prepared, None, None).unwrap();
        assert_eq!(added, 0);
        assert!(impacts.library);
        assert!(!impacts.structure);
        let stats = store.library_root_stats(&["/outside".into()]).unwrap();
        assert_eq!(stats[0].total, 1);
        assert_eq!(stats[0].file_size, 1234);
    }

    #[test]
    fn source_refresh_reports_only_effective_history_changes_for_played_members() {
        for scan in [false, true] {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let apply = |source: ScanRecord| {
                if scan {
                    store
                        .apply_scan(&ScanResult {
                            records: vec![std::sync::Arc::new(source)],
                            ..Default::default()
                        })
                        .unwrap()
                } else {
                    store
                        .admit_external_queue(
                            &ExternalAdmission::from_test_records(vec![source], vec![0]),
                            None,
                            None,
                        )
                        .unwrap()
                        .1
                }
            };
            let history_values = || {
                store
                    .history(10)
                    .unwrap()
                    .into_iter()
                    .map(|entry| {
                        (
                            entry.track_id,
                            entry.title,
                            entry.play_count,
                            entry.played_at,
                            entry.favorite,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let mut source = record("/history.wav");
            assert!(!apply(source.clone()).history);
            let id = store
                .track_id_for_identity(super::super::TrackIdentity::File(&source.path))
                .unwrap()
                .unwrap();
            source.media.title = "Unplayed title".into();
            assert!(!apply(source.clone()).history);
            assert!(store.history(10).unwrap().is_empty());
            store.mark_played(id, 42).unwrap();
            let history = history_values();
            source.size += 1;
            source.modified_ns += 1;
            source.media.codec = "flac".into();
            source.media.bitrate_bps = Some(512_000);
            let technical = apply(source.clone());
            assert!(technical.library);
            assert!(!technical.history);
            assert_eq!(history_values(), history);
            source.media.title = "Played title".into();
            assert!(apply(source.clone()).history);
            assert_eq!(store.history(10).unwrap()[0].title, "Played title");
            store
                .edit_track(id, Some("Personal title"), None, None, None)
                .unwrap();
            let overridden = history_values();
            source.media.title = "Hidden title".into();
            assert!(!apply(source).history);
            assert_eq!(history_values(), overridden);
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
        assert_eq!(inserted, 3);
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
                .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                    "/outside/two.wav"
                )))
                .unwrap()
                .is_none()
        );
        let before = store.queue_state().unwrap();
        store.conn.execute_batch("CREATE TEMP TRIGGER reject_admission BEFORE INSERT ON playlist_entries WHEN new.playlist_id=0 BEGIN SELECT RAISE(ABORT,'rejected queue write'); END;").unwrap();
        assert!(store.admit_external_queue(&next, None, None).is_err());
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                    "/outside/two.wav"
                )))
                .unwrap()
                .is_none()
        );
        assert_eq!(store.track_count().unwrap(), 1);
        assert_eq!(store.queue_state().unwrap(), before);
    }

    #[test]
    fn unresolved_occurrence_rolls_back_registration_and_allows_retry() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let existing = admission(&["/outside/old.wav"], vec![0]);
        store.admit_external_queue(&existing, None, None).unwrap();
        let current_id = store.queue_ids().unwrap()[0];
        store.set_queue_current(Some(current_id)).unwrap();
        let before = store.queue_state().unwrap();
        let prepared = admission(
            &["/outside/first.wav", "/outside/unresolved.wav"],
            vec![0, 1, 0],
        );
        store.conn.execute_batch("CREATE TEMP TRIGGER skip_external_identity BEFORE INSERT ON tracks WHEN new.path='/outside/unresolved.wav' BEGIN SELECT RAISE(IGNORE); END;").unwrap();

        assert!(
            store
                .admit_external_queue(&prepared, Some(0), None)
                .is_err()
        );
        assert_eq!(store.track_count().unwrap(), 1);
        assert_eq!(store.queue_state().unwrap(), before);
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                    "/outside/first.wav"
                )))
                .unwrap()
                .is_none()
        );

        store
            .conn
            .execute_batch("DROP TRIGGER skip_external_identity;")
            .unwrap();
        let (inserted, _, added) = store
            .admit_external_queue(&prepared, Some(0), Some(current_id))
            .unwrap();
        assert_eq!(inserted, 3);
        assert_eq!(added, 2);
        assert_eq!(store.queue_id_at(3).unwrap(), Some(current_id));
        assert_eq!(store.queue_state().unwrap().current_id, Some(current_id));
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
        assert_eq!(inserted, 2);
        let inserted = [
            store.queue_id_at(1).unwrap().unwrap(),
            store.queue_id_at(2).unwrap().unwrap(),
        ];
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
        let cue = CueRecord::registered(
            std::sync::Arc::new(record("/outside/shared.wav")),
            CueSegment {
                sheet: "/outside/shared.cue".into(),
                number: 1,
                start_frame: 0,
                end_frame: None,
            },
            RegisteredCueMetadata {
                title: "Registered segment".into(),
                artist: String::new(),
                album: String::new(),
                track_number: None,
                disc_number: None,
                release_date: None,
            },
            Some(0.01),
        );
        let ordinary = record("/outside/existing.wav");
        let absent = record("/outside/absent.wav");
        store
            .apply_scan(&ScanResult {
                segments: vec![cue],
                records: (vec![ordinary, absent])
                    .into_iter()
                    .map(std::sync::Arc::new)
                    .collect(),
                ..ScanResult::default()
            })
            .unwrap();
        let ordinary_id = store
            .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                "/outside/existing.wav",
            )))
            .unwrap()
            .unwrap();
        store.conn.execute("UPDATE tracks SET title_override='Keep title',favorite=1,play_count=7,missing=1 WHERE id=?1", [ordinary_id]).unwrap();
        let mut copied = record("/outside/copied.wav");
        copied.fingerprint = record("/outside/absent.wav").fingerprint;
        let prepared = ExternalAdmission::from_test_records(
            vec![
                record("/outside/shared.wav"),
                record("/outside/existing.wav"),
                copied,
            ],
            vec![1, 0, 1, 2],
        );
        let (_, _, added) = store.admit_external_queue(&prepared, None, None).unwrap();
        assert_eq!(added, 2);
        let track = store.track(ordinary_id).unwrap().unwrap();
        assert_eq!(track.title, "Keep title");
        assert!(track.favorite);
        assert_eq!(track.play_count, 7);
        assert!(!track.missing);
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::Cue(
                    Path::new("/outside/shared.cue"),
                    1
                ))
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                    "/outside/shared.wav"
                )))
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .track_id_for_identity(crate::store::TrackIdentity::File(Path::new(
                    "/outside/absent.wav"
                )))
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
        let mut updated = record("/outside/existing.wav");
        updated.size = 1234;
        updated.modified_ns = 5678;
        updated.fingerprint = Some(blake3::hash(b"fresh encoded content").to_hex().to_string());
        let media = &mut updated.media;
        media.title = "Fresh raw title".into();
        media.artist = "Fresh raw artist".into();
        media.album = "Fresh raw album".into();
        media.duration = Some(2.5);
        media.codec = "flac".into();
        media.sample_rate = 96_000;
        media.channels = 2;
        media.bitrate_bps = Some(512_000);
        media.bitrate_exact = true;
        prepared = ExternalAdmission::from_test_records(vec![updated], vec![0, 0]);
        let (inserted, impacts, added) = store
            .admit_external_queue(&prepared, None, Some(old_queue[0]))
            .unwrap();
        assert_eq!(inserted, 2);
        let inserted = [
            store.queue_id_at(1).unwrap().unwrap(),
            store.queue_id_at(2).unwrap().unwrap(),
        ];
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
        assert_eq!(known.fingerprint, prepared.records()[0].fingerprint);
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
