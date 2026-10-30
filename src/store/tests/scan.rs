use crate::library::{CueRecord, RegisteredCueMetadata, ScanRecord, ScanResult};
use crate::store::Store;
use crate::store::scan::{PROBE_VERSION, ScanImpacts};
use rivu_audio::MediaInfo;
use rivu_protocol::model::CueSegment;
use std::path::Path;

fn record() -> ScanRecord {
    ScanRecord {
        path: "/music/tagged.wav".into(),
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
            bitrate_exact: true,
            track_number: Some(1),
            disc_number: Some(1),
            bits_per_sample: Some(16),
            release_date: Some("2024".into()),
        },
    }
}

fn cue_metadata() -> RegisteredCueMetadata {
    RegisteredCueMetadata {
        title: "Title".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        track_number: Some(1),
        disc_number: Some(1),
        release_date: Some("2024".into()),
    }
}

fn queued(record: ScanRecord) -> (Store, ScanResult, i64) {
    queued_scan(ScanResult {
        roots: vec!["/music".into()],
        records: vec![std::sync::Arc::new(record)],
        ..ScanResult::default()
    })
}

fn queued_scan(scan: ScanResult) -> (Store, ScanResult, i64) {
    let store = Store::open(Path::new(":memory:")).unwrap();
    assert!(store.apply_scan(&scan).unwrap().changed);
    store
        .enqueue_sources(&["/music".into()], &[], None)
        .unwrap();
    let id = store.queue_page(None, 0, 10).unwrap().rows[0].track_id;
    (store, scan, id)
}

#[test]
fn duration_admission_requires_known_positive_duration_strictly_below_threshold() {
    for threshold in [0, 60] {
        for duration in [
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
            Some(-1.0),
            Some(0.0),
            Some(59.999),
            Some(60.0),
            Some(60.001),
        ] {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let mut record = record();
            record.media.duration = duration;
            let scan = ScanResult {
                min_duration_secs: threshold,
                records: (vec![record])
                    .into_iter()
                    .map(std::sync::Arc::new)
                    .collect(),
                ..ScanResult::default()
            };
            let skipped = threshold != 0 && duration == Some(59.999);
            let impacts = store.apply_scan(&scan).unwrap();
            assert_eq!(impacts.skipped_short_tracks, usize::from(skipped));
            assert_eq!(impacts.changed, !skipped);
            assert_eq!(
                store
                    .track_id_for_identity(crate::store::TrackIdentity::File(&scan.records[0].path))
                    .unwrap()
                    .is_some(),
                !skipped,
                "threshold={threshold}, duration={duration:?}"
            );
        }
    }
}

#[test]
fn successful_cue_admits_short_whole_and_segment_with_independent_state() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut whole = record();
    whole.media.duration = Some(2.0);
    let whole = std::sync::Arc::new(whole);
    let cue = CueRecord::registered(
        whole.clone(),
        CueSegment {
            sheet: "/music/album.cue".into(),
            number: 1,
            start_frame: 0,
            end_frame: Some(75),
        },
        cue_metadata(),
        Some(1.0),
    );
    let mut ordinary = whole.as_ref().clone();
    ordinary.path = "/music/ordinary.wav".into();
    let mut scan = ScanResult {
        roots: vec!["/music".into()],
        duration_exempt_sources: vec![whole.path.clone()],
        min_duration_secs: 60,
        records: vec![whole, std::sync::Arc::new(ordinary)],
        segments: vec![cue],
        ..ScanResult::default()
    };
    assert_eq!(store.apply_scan(&scan).unwrap().skipped_short_tracks, 1);
    let whole_id = store
        .track_id_for_identity(crate::store::scan::TrackIdentity::File(
            &scan.records[0].path,
        ))
        .unwrap()
        .unwrap();
    let cue_id = store
        .track_id_for_identity(crate::store::scan::TrackIdentity::Cue(
            Path::new("/music/album.cue"),
            1,
        ))
        .unwrap()
        .unwrap();
    assert_ne!(whole_id, cue_id);
    store.set_favorite(&[whole_id], true).unwrap();
    store
        .edit_track(cue_id, Some("Owned segment"), None, None, None)
        .unwrap();
    let playlist = store
        .create_playlist_with_tracks("Mixed identities", &[cue_id, whole_id, cue_id])
        .unwrap();
    scan.records.reverse();
    assert_eq!(store.apply_scan(&scan).unwrap().skipped_short_tracks, 1);
    let whole = store.track(whole_id).unwrap().unwrap();
    let cue = store.track(cue_id).unwrap().unwrap();
    assert!(whole.favorite);
    assert!(!cue.favorite);
    assert_eq!(cue.title, "Owned segment");
    assert_eq!(whole.duration, Some(2.0));
    assert_eq!(cue.duration, Some(1.0));
    assert_eq!(
        store.playlist_track_ids(playlist, 10).unwrap(),
        [cue_id, whole_id, cue_id]
    );
}

#[test]
fn short_existing_ordinary_and_cue_tracks_refresh_without_losing_identity() {
    for cue in [false, true] {
        let source = std::sync::Arc::new(record());
        let initial_cue = CueSegment {
            sheet: "/music/album.cue".into(),
            number: 1,
            start_frame: 0,
            end_frame: Some(4500),
        };
        let initial = if cue {
            ScanResult {
                roots: vec!["/music".into()],
                segments: vec![CueRecord::registered(
                    source.clone(),
                    initial_cue.clone(),
                    cue_metadata(),
                    Some(60.0),
                )],
                ..ScanResult::default()
            }
        } else {
            ScanResult {
                roots: vec!["/music".into()],
                records: vec![source.clone()],
                ..ScanResult::default()
            }
        };
        let (store, mut scan, id) = queued_scan(initial);
        scan.min_duration_secs = 120;
        let mut refreshed = source.as_ref().clone();
        refreshed.media.duration = Some(1.0);
        refreshed.media.title = "Refreshed short track".into();
        refreshed.modified_ns += 1;
        let refreshed_cue = cue.then_some(CueSegment {
            end_frame: Some(75),
            ..initial_cue
        });
        if let Some(segment) = &refreshed_cue {
            let mut metadata = cue_metadata();
            metadata.title = "Refreshed short track".into();
            scan.segments = vec![CueRecord::registered(
                std::sync::Arc::new(refreshed),
                segment.clone(),
                metadata,
                Some(1.0),
            )];
        } else {
            scan.records = vec![std::sync::Arc::new(refreshed)];
        }
        let impacts = store.apply_scan(&scan).unwrap();
        assert_eq!(impacts.skipped_short_tracks, 0);
        assert!(impacts.changed);
        let track = store.track(id).unwrap().unwrap();
        assert!(!track.missing);
        assert_eq!(track.title, "Refreshed short track");
        assert_eq!(track.duration, Some(1.0));
        assert_eq!(track.cue, refreshed_cue);
        assert_eq!(store.queue_page(None, 0, 10).unwrap().rows[0].track_id, id);
    }
}

#[test]
fn short_track_relocation_keeps_the_existing_resolution_contract() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = record();
    record.path = directory.path().join("absent-old.wav");
    let mut scan = ScanResult {
        records: (vec![record])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        ..ScanResult::default()
    };
    store.apply_scan(&scan).unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&scan.records[0].path))
        .unwrap()
        .unwrap();
    scan.min_duration_secs = 120;
    std::sync::Arc::make_mut(&mut scan.records[0]).path = directory.path().join("new.wav");
    std::sync::Arc::make_mut(&mut scan.records[0]).media.title = "Moved short track".into();
    let impacts = store.apply_scan(&scan).unwrap();
    assert_eq!(impacts.skipped_short_tracks, 0);
    assert_eq!(store.track_count().unwrap(), 1);
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.path, scan.records[0].path);
    assert_eq!(track.title, "Moved short track");
    assert!(!track.missing);
}

#[test]
fn inserts_report_identity_and_only_applicable_duplicate_impacts() {
    for (cue, fingerprint) in [(false, true), (false, false), (true, true)] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut record = record();
        if !fingerprint {
            record.fingerprint = None;
        }
        let mut scan = ScanResult {
            roots: vec!["/music".into()],
            ..ScanResult::default()
        };
        if cue {
            scan.segments.push(CueRecord::registered(
                std::sync::Arc::new(record),
                CueSegment {
                    sheet: "/music/album.cue".into(),
                    number: 1,
                    start_frame: 0,
                    end_frame: Some(4500),
                },
                cue_metadata(),
                Some(60.0),
            ));
        } else {
            scan.records.push(std::sync::Arc::new(record));
        }
        assert_eq!(
            store.apply_scan(&scan).unwrap(),
            ScanImpacts {
                changed: true,
                library: true,
                albums: true,
                structure: true,
                duplicate_groups: !cue && fingerprint,
                duplicate_members: !cue && fingerprint,
                ..ScanImpacts::default()
            }
        );
        assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
    }
}

#[test]
fn verification_rescans_invalidate_library_stats_only_when_size_changes() {
    let (store, mut scan, id) = queued(record());
    let track = store.track(id).unwrap().unwrap();
    let page = store.queue_page(None, 0, 10).unwrap();
    let state = store.queue_state().unwrap();
    for mtime_only in [true, false] {
        if mtime_only {
            std::sync::Arc::make_mut(&mut scan.records[0]).modified_ns += 1;
        } else {
            std::sync::Arc::make_mut(&mut scan.records[0]).size += 1;
        }
        assert_eq!(
            store.apply_scan(&scan).unwrap(),
            ScanImpacts {
                changed: true,
                library: !mtime_only,
                duplicate_members: true,
                ..ScanImpacts::default()
            }
        );
        assert_eq!(
            store.track_cache.borrow().entries.contains_key(&id),
            mtime_only
        );
        assert_eq!(store.track(id).unwrap().unwrap(), track);
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
        let member = store
            .duplicate_members_page(scan.records[0].fingerprint.as_deref().unwrap(), 0, 10)
            .unwrap()
            .rows
            .remove(0);
        assert_eq!(member.file_size, scan.records[0].size);
        assert_eq!(member.modified_ns, scan.records[0].modified_ns);
        let stats = store.library_root_stats(&["/music".into()]).unwrap();
        assert_eq!(stats[0].file_size, scan.records[0].size);
        assert_eq!(stats[0].total, 1);
        assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
    }
}

#[test]
fn cue_size_only_rescans_invalidate_library_stats_without_fingerprints() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut source = record();
    source.fingerprint = None;
    let mut scan = ScanResult {
        segments: vec![CueRecord::registered(
            std::sync::Arc::new(source),
            CueSegment {
                sheet: "/music/album.cue".into(),
                number: 1,
                start_frame: 0,
                end_frame: Some(4500),
            },
            cue_metadata(),
            Some(60.0),
        )],
        ..ScanResult::default()
    };
    store.apply_scan(&scan).unwrap();
    std::sync::Arc::make_mut(&mut scan.segments[0].source).size += 1;
    let impacts = store.apply_scan(&scan).unwrap();
    assert!(impacts.library);
    assert!(!impacts.structure);
    assert!(!impacts.duplicate_members);
    assert_eq!(
        store.library_root_stats(&["/music".into()]).unwrap()[0].file_size,
        scan.segments[0].source.size
    );
}

#[test]
fn ordinary_fingerprint_changes_affect_grouping_but_not_structure() {
    let (store, mut scan, id) = queued(record());
    for fingerprint in [
        None,
        Some(blake3::hash(b"replacement").to_hex().to_string()),
    ] {
        std::sync::Arc::make_mut(&mut scan.records[0]).fingerprint = fingerprint;
        assert_eq!(
            store.apply_scan(&scan).unwrap(),
            ScanImpacts {
                changed: true,
                library: true,
                albums: true,
                duplicate_groups: true,
                ..ScanImpacts::default()
            }
        );
        assert_eq!(
            store.track(id).unwrap().unwrap().fingerprint,
            scan.records[0].fingerprint
        );
    }
}

#[test]
fn library_changes_do_not_proxy_for_queue_or_album_effects() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut first = record();
    first.media.title = "First".into();
    first.media.track_number = Some(1);
    let mut second = first.clone();
    second.path = "/music/second.wav".into();
    second.media.title = "Second".into();
    second.media.track_number = Some(2);
    let mut plain = first.clone();
    plain.path = "/other/plain.wav".into();
    plain.media.album.clear();
    let mut scan = ScanResult {
        records: (vec![first, second, plain])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        ..ScanResult::default()
    };
    store.apply_scan(&scan).unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&scan.records[0].path))
        .unwrap()
        .unwrap();
    store.enqueue_tracks(&[id]).unwrap();

    std::sync::Arc::make_mut(&mut scan.records[1])
        .media
        .release_date = Some("2042-01".into());
    let member = store.apply_scan(&scan).unwrap();
    assert!(member.changed && member.library && !member.queue && !member.albums);
    std::sync::Arc::make_mut(&mut scan.records[2]).media.title = "Retitled plain track".into();
    let plain = store.apply_scan(&scan).unwrap();
    assert!(plain.changed && plain.library && !plain.queue && !plain.albums);
    std::sync::Arc::make_mut(&mut scan.records[1]).media.title = "Retitled album member".into();
    let member = store.apply_scan(&scan).unwrap();
    assert!(member.changed && member.library && !member.queue && member.albums);
    std::sync::Arc::make_mut(&mut scan.records[0])
        .media
        .release_date = Some("2042-01".into());
    let representative = store.apply_scan(&scan).unwrap();
    assert!(
        representative.changed
            && representative.library
            && !representative.queue
            && representative.albums
    );
    std::sync::Arc::make_mut(&mut scan.records[0]).media.title = "Retitled queued track".into();
    let queued = store.apply_scan(&scan).unwrap();
    assert!(queued.changed && queued.library && queued.queue && queued.albums);
    assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
}

#[test]
fn identical_rescans_keep_queue_content_stable() {
    let (store, scan, id) = queued(record());
    store.set_favorite(&[id], true).unwrap();
    store.mark_played(id, 123).unwrap();
    store.increment_play_count(id).unwrap();
    store
        .edit_track(id, Some("Override"), None, None, None)
        .unwrap();
    let page = store.queue_page(None, 0, 10).unwrap();
    store.set_queue_current(Some(page.rows[0].id)).unwrap();
    let state = store.queue_state().unwrap();
    let track = store.track(id).unwrap().unwrap();
    for _ in 0..2 {
        assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
        assert_eq!(store.queue_state().unwrap(), state);
        assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
        assert_eq!(store.track(id).unwrap().unwrap(), track);
    }
}

#[test]
fn changed_scan_metadata_invalidates_queue_without_losing_entry_identity() {
    let (store, mut scan, id) = queued(record());
    let before = store.queue_page(None, 0, 10).unwrap().rows.remove(0);
    let state = store.queue_state().unwrap();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.title = "Changed title".into();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.artist = "Changed artist".into();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.album = "Changed album".into();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.codec = "flac".into();
    assert_eq!(
        store.apply_scan(&scan).unwrap(),
        ScanImpacts {
            changed: true,
            library: true,
            queue: true,
            albums: true,
            ..ScanImpacts::default()
        }
    );
    assert_eq!(store.queue_state().unwrap(), state);
    let after = store.queue_page(None, 0, 10).unwrap().rows.remove(0);
    assert_eq!(
        (after.id, after.track_id, after.position),
        (before.id, id, before.position)
    );
    assert_eq!(after.track.title, "Changed title");
    assert_eq!(after.track.artist, "Changed artist");
    assert_eq!(after.track.album, "Changed album");
    assert_eq!(after.track.codec, "flac");
    let state = store.queue_state().unwrap();
    assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
    assert_eq!(store.queue_state().unwrap(), state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap().rows[0], after);
}

#[test]
fn audio_source_rescans_refresh_ordinary_and_cue_row_projections() {
    let changes: [fn(&mut MediaInfo); 5] = [
        |media| media.channels = 1,
        |media| media.sample_rate = 44_100,
        |media| media.bits_per_sample = Some(24),
        |media| media.bitrate_bps = Some(1_024_000),
        |media| media.bitrate_exact = false,
    ];
    for cue in [false, true] {
        let mut source = record();
        source.fingerprint = None;
        let source = std::sync::Arc::new(source);
        let mut scan = ScanResult {
            roots: vec!["/music".into()],
            ..ScanResult::default()
        };
        if cue {
            scan.segments.push(CueRecord::registered(
                source,
                CueSegment {
                    sheet: "/music/album.cue".into(),
                    number: 1,
                    start_frame: 0,
                    end_frame: Some(4500),
                },
                cue_metadata(),
                Some(60.0),
            ));
        } else {
            scan.records.push(source);
        }
        let (store, mut scan, id) = queued_scan(scan);
        let playlist = store
            .create_playlist_with_tracks("Audio facts", &[id])
            .unwrap();
        for change in changes {
            let source = if cue {
                std::sync::Arc::make_mut(&mut scan.segments[0].source)
            } else {
                std::sync::Arc::make_mut(&mut scan.records[0])
            };
            change(&mut source.media);
            source.modified_ns += 1;
            let expected = source.media.clone();
            let impacts = store.apply_scan(&scan).unwrap();
            assert!(impacts.library && impacts.queue && impacts.albums);
            assert!(!impacts.structure);
            let track = store.track(id).unwrap().unwrap();
            let queued = store.queue_page(None, 0, 10).unwrap().rows.remove(0).track;
            let member = store
                .playlist_entries_page(playlist, None, 0, 10)
                .unwrap()
                .rows
                .remove(0);
            for (channels, sample_rate, bits_per_sample, bitrate_bps, bitrate_exact) in [
                (
                    track.channels,
                    track.sample_rate,
                    track.bits_per_sample,
                    track.bitrate_bps,
                    track.bitrate_exact,
                ),
                (
                    queued.channels,
                    queued.sample_rate,
                    queued.bits_per_sample,
                    queued.bitrate_bps,
                    queued.bitrate_exact,
                ),
                (
                    member.channels,
                    member.sample_rate,
                    member.bits_per_sample,
                    member.bitrate_bps,
                    member.bitrate_exact,
                ),
            ] {
                assert_eq!(
                    (
                        channels,
                        sample_rate,
                        bits_per_sample,
                        bitrate_bps,
                        bitrate_exact
                    ),
                    (
                        expected.channels,
                        expected.sample_rate,
                        expected.bits_per_sample,
                        expected.bitrate_bps,
                        expected.bitrate_exact
                    )
                );
            }
            assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
        }
    }
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
    let page = store.queue_page(None, 0, 10).unwrap();
    let state = store.queue_state().unwrap();
    store.track(id).unwrap();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.title = "New raw title".into();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.artist = "New raw artist".into();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.album = "New raw album".into();
    assert_eq!(
        store.apply_scan(&scan).unwrap(),
        ScanImpacts {
            changed: true,
            ..ScanImpacts::default()
        }
    );
    assert!(store.track_cache.borrow().entries.contains_key(&id));
    assert_eq!(store.queue_state().unwrap(), state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
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
fn equal_to_raw_and_repeated_overrides_keep_queue_values_stable() {
    let (store, mut scan, id) = queued(record());
    let page = store.queue_page(None, 0, 10).unwrap();
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
            assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
        }
    }
    // A later rescan proves equal overrides were retained, not discarded.
    std::sync::Arc::make_mut(&mut scan.records[0]).media.title = "Changed raw title".into();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.artist = "Changed raw artist".into();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.album = "Changed raw album".into();
    store.apply_scan(&scan).unwrap();
    assert_eq!(store.queue_state().unwrap(), state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
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
        let before = store.queue_page(None, 0, 10).unwrap().rows.remove(0);
        let state = store.queue_state().unwrap();
        assert!(
            store
                .edit_track(id, title, artist, album, None)
                .unwrap()
                .queue
        );
        assert_eq!(store.queue_state().unwrap(), state);
        let after = store.queue_page(None, 0, 10).unwrap().rows.remove(0);
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
        assert_eq!(store.queue_page(None, 0, 10).unwrap().rows[0], after);
    }
}

#[test]
fn missing_transitions_invalidate_once_and_recovery_preserves_queue_entry() {
    let (store, scan, id) = queued(record());
    let page = store.queue_page(None, 0, 10).unwrap();
    let initial = store.queue_state().unwrap();
    let missing = ScanResult {
        roots: scan.roots.clone(),
        ..ScanResult::default()
    };
    let expected = ScanImpacts {
        changed: true,
        library: true,
        queue: true,
        albums: true,
        duplicate_groups: true,
        ..ScanImpacts::default()
    };
    assert_eq!(store.apply_scan(&missing).unwrap(), expected);
    let state = store.queue_state().unwrap();
    assert_eq!(state, initial);
    let row = store.queue_page(None, 0, 10).unwrap().rows.remove(0);
    assert_eq!(row.id, page.rows[0].id);
    assert_eq!(row.track_id, id);
    assert!(row.track.missing);
    assert_eq!(store.apply_scan(&missing).unwrap(), ScanImpacts::default());
    assert_eq!(store.queue_state().unwrap(), state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap().rows[0], row);
    assert_eq!(store.apply_scan(&scan).unwrap(), expected);
    let recovered = store.queue_state().unwrap();
    assert_eq!(recovered, state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
    assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
    assert_eq!(store.queue_state().unwrap(), recovered);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
}

#[test]
fn optional_scan_metadata_clears_and_restores_queue_values() {
    let (store, mut scan, id) = queued(record());
    let original = scan.records[0].clone();
    let state = store.queue_state().unwrap();
    std::sync::Arc::make_mut(&mut scan.records[0])
        .media
        .duration = None;
    std::sync::Arc::make_mut(&mut scan.records[0])
        .media
        .bitrate_bps = None;
    std::sync::Arc::make_mut(&mut scan.records[0]).fingerprint = None;
    assert!(store.apply_scan(&scan).unwrap().queue);
    let cleared = store.queue_state().unwrap();
    assert_eq!(cleared, state);
    let row = store.queue_page(None, 0, 10).unwrap().rows.remove(0);
    assert_eq!(row.track.duration, None);
    assert_eq!(row.track.bitrate_bps, None);
    assert_eq!(store.track(id).unwrap().unwrap().fingerprint, None);
    store.apply_scan(&scan).unwrap();
    assert_eq!(store.queue_state().unwrap(), cleared);
    assert_eq!(store.queue_page(None, 0, 10).unwrap().rows[0], row);
    scan.records[0] = original;
    assert!(store.apply_scan(&scan).unwrap().queue);
    assert_eq!(store.queue_state().unwrap(), cleared);
    let row = store.queue_page(None, 0, 10).unwrap().rows.remove(0);
    assert_eq!(row.track.duration, Some(60.0));
    assert_eq!(row.track.bitrate_bps, Some(1_536_000));
}

#[test]
fn scan_metadata_outside_queue_rows_refreshes_without_queue_invalidation() {
    let cue = CueSegment {
        sheet: "/music/album.cue".into(),
        number: 1,
        start_frame: 0,
        end_frame: Some(4500),
    };
    let (store, mut scan, id) = queued_scan(ScanResult {
        roots: vec!["/music".into()],
        segments: vec![CueRecord::registered(
            std::sync::Arc::new(record()),
            cue,
            cue_metadata(),
            Some(60.0),
        )],
        ..ScanResult::default()
    });
    let page = store.queue_page(None, 0, 10).unwrap();
    let state = store.queue_state().unwrap();
    let source = std::sync::Arc::make_mut(&mut scan.segments[0].source);
    source.fingerprint = None;
    source.size += 1;
    source.modified_ns += 1;
    let mut metadata = cue_metadata();
    metadata.track_number = None;
    metadata.disc_number = None;
    metadata.release_date = None;
    let changed_cue = CueSegment {
        sheet: "/music/album.cue".into(),
        number: 1,
        start_frame: 75,
        end_frame: None,
    };
    scan.segments[0] = CueRecord::registered(
        scan.segments[0].source.clone(),
        changed_cue.clone(),
        metadata,
        Some(60.0),
    );
    assert_eq!(
        store.apply_scan(&scan).unwrap(),
        ScanImpacts {
            changed: true,
            library: true,
            albums: true,
            ..ScanImpacts::default()
        }
    );
    assert_eq!(store.queue_state().unwrap(), state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.path, scan.segments[0].source.path);
    assert_eq!(track.fingerprint, None);
    assert_eq!(track.cue, Some(changed_cue));
    assert_eq!((track.track_number, track.disc_number), (None, None));
    assert_eq!(track.release_date, None);
    let known = store.stored_scan_files(&scan.roots).unwrap().remove(0);
    assert_eq!(known.size, scan.segments[0].source.size);
    assert_eq!(known.modified_ns, scan.segments[0].source.modified_ns);
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
    assert_eq!(
        store.apply_scan(&scan).unwrap(),
        ScanImpacts {
            changed: true,
            ..ScanImpacts::default()
        }
    );
    assert!(store.track_cache.borrow().entries.contains_key(&id));
    assert!(
        store.stored_scan_files(&scan.roots).unwrap()[0]
            .media
            .is_some()
    );
    assert_eq!(store.queue_state().unwrap(), state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
}

#[test]
fn cue_source_rename_invalidates_queue_rows_without_changing_occurrences_or_current() {
    let (store, mut scan, id) = queued_scan(ScanResult {
        roots: vec!["/music".into()],
        segments: vec![CueRecord::registered(
            std::sync::Arc::new(record()),
            CueSegment {
                sheet: "/music/album.cue".into(),
                number: 1,
                start_frame: 0,
                end_frame: Some(4500),
            },
            cue_metadata(),
            Some(60.0),
        )],
        ..ScanResult::default()
    });
    store.enqueue_tracks(&[id]).unwrap();
    let mut expected = store.queue_page(None, 0, 10).unwrap();
    store.set_queue_current(Some(expected.rows[1].id)).unwrap();
    let before = store.queue_state().unwrap();
    std::sync::Arc::make_mut(&mut scan.segments[0].source).path = "/music/renamed.wav".into();
    assert_eq!(
        store.apply_scan(&scan).unwrap(),
        ScanImpacts {
            changed: true,
            library: true,
            queue: true,
            albums: true,
            structure: true,
            ..ScanImpacts::default()
        }
    );
    let after = store.queue_state().unwrap();
    assert_eq!(
        (after.total, after.current_id, after.current_position),
        (before.total, before.current_id, before.current_position)
    );
    for row in &mut expected.rows {
        row.track.path = scan.segments[0].source.path.clone();
    }
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), expected);
    assert_eq!(
        store.track(id).unwrap().unwrap().path,
        scan.segments[0].source.path
    );
    assert_eq!(store.apply_scan(&scan).unwrap(), ScanImpacts::default());
    assert_eq!(store.queue_state().unwrap(), after);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), expected);
}

#[test]
fn unqueued_metadata_changes_do_not_invalidate_queue() {
    let (store, mut scan, _) = queued(record());
    let mut other = record();
    other.path = "/music/other.wav".into();
    other.fingerprint = Some(blake3::hash(b"other").to_hex().to_string());
    scan.records.push(std::sync::Arc::new(other));
    store.apply_scan(&scan).unwrap();
    let page = store.queue_page(None, 0, 10).unwrap();
    let state = store.queue_state().unwrap();
    std::sync::Arc::make_mut(&mut scan.records[1]).media.title = "Unqueued title".into();
    store.apply_scan(&scan).unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&scan.records[1].path))
        .unwrap()
        .unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().title, "Unqueued title");
    assert_eq!(store.queue_state().unwrap(), state);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page);
}

#[test]
fn source_lookup_prefers_ordinary_then_lowest_cue_id_even_when_missing() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let path = Path::new("/music/shared.wav");
    store
        .conn
        .execute_batch(
            "INSERT INTO tracks(id,path,cue_sheet,cue_number,cue_start_frame,missing) VALUES
             (20,'/music/shared.wav','/music/a.cue',1,0,0),
             (5,'/music/shared.wav','/music/z.cue',2,75,1),
             (30,'/music/shared.wav',NULL,NULL,NULL,0);",
        )
        .unwrap();
    assert!(store.known_source(path).unwrap().unwrap().cue.is_none());
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(path))
            .unwrap(),
        Some(30)
    );
    store
        .conn
        .execute("UPDATE tracks SET missing=1 WHERE id=30", [])
        .unwrap();
    assert!(store.known_source(path).unwrap().unwrap().cue.is_none());
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(path))
            .unwrap(),
        None
    );
    store
        .conn
        .execute("DELETE FROM tracks WHERE id=30", [])
        .unwrap();
    let cue = store.known_source(path).unwrap().unwrap().cue.unwrap();
    assert_eq!(cue.sheet, Path::new("/music/z.cue"));
    assert_eq!(cue.number, 2);
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(path))
            .unwrap(),
        None
    );
}

#[test]
fn source_metadata_prefers_whole_file_and_falls_back_to_cue_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.wav");
    std::fs::write(&path, []).unwrap();
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut whole = record();
    whole.path = path.clone();
    let whole = std::sync::Arc::new(whole);
    let segment = CueRecord::registered(
        whole.clone(),
        CueSegment {
            sheet: directory.path().join("album.cue"),
            number: 1,
            start_frame: 0,
            end_frame: Some(4500),
        },
        cue_metadata(),
        Some(60.0),
    );
    let mut records = vec![whole];
    for index in 0..128 {
        let mut unrelated = record();
        unrelated.path = directory.path().join(format!("other/{index}.wav"));
        unrelated.fingerprint = Some(
            blake3::hash(index.to_string().as_bytes())
                .to_hex()
                .to_string(),
        );
        records.push(std::sync::Arc::new(unrelated));
    }
    assert!(
        store
            .apply_scan(&ScanResult {
                records,
                segments: vec![segment],
                ..ScanResult::default()
            })
            .unwrap()
            .changed
    );
    let known = store.known_source(&path).unwrap().unwrap();
    assert!(known.cue.is_none());
    assert!(
        store
            .known_source(&directory.path().join("missing.wav"))
            .unwrap()
            .is_none()
    );
    store
        .conn
        .execute(
            "DELETE FROM tracks WHERE path=?1 AND cue_sheet IS NULL",
            [path.to_str().unwrap()],
        )
        .unwrap();
    let segment = store.known_source(&path).unwrap().unwrap();
    assert!(segment.cue.is_some());
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
            records: (vec![original.clone(), unrelated.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&old_path))
        .unwrap()
        .unwrap();
    let unrelated_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&unrelated.path))
        .unwrap()
        .unwrap();
    store.set_favorite(&[id], true).unwrap();
    original.path = requested.join("音楽.wav");
    let scan = ScanResult {
        roots: vec![requested.clone()],
        records: (vec![original.clone()])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        ..ScanResult::default()
    };
    assert_eq!(
        store.apply_scan(&scan).unwrap(),
        ScanImpacts {
            changed: true,
            library: true,
            albums: true,
            structure: true,
            ..ScanImpacts::default()
        }
    );
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
            .unwrap(),
        Some(id)
    );
    assert!(store.track(id).unwrap().unwrap().favorite);
    assert!(!store.track(unrelated_id).unwrap().unwrap().missing);
    assert!(!store.apply_scan(&scan).unwrap().changed);
    let absent = ScanResult {
        roots: vec![requested],
        ..ScanResult::default()
    };
    assert!(store.apply_scan(&absent).unwrap().changed);
    assert!(store.track(id).unwrap().unwrap().missing);
    assert!(!store.track(unrelated_id).unwrap().unwrap().missing);
    assert!(!store.apply_scan(&absent).unwrap().changed);
    assert!(store.apply_scan(&scan).unwrap().changed);
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
            records: (vec![original.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
        .unwrap()
        .unwrap();
    store.enqueue_sources(&[], &[id], None).unwrap();
    let before = store.track(id).unwrap().unwrap();
    let mut replacement = original.clone();
    replacement.path = directory.path().join("new/source.wav");
    let mut scan = ScanResult {
        roots: vec![replacement.path.clone()],
        records: (vec![replacement.clone()])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        uncertainties: vec![crate::library::ScanUncertainty::File(original.path.clone())],
        ..ScanResult::default()
    };
    assert!(store.apply_scan(&scan).unwrap().changed);
    assert_eq!(store.track(id).unwrap().unwrap(), before);
    let new_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&replacement.path))
        .unwrap()
        .unwrap();
    assert_ne!(id, new_id);
    let queue = store.queue_state().unwrap();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.title = "Must roll back".into();
    let mut invalid = record();
    invalid.path = directory.path().join("new/invalid.wav");
    invalid.fingerprint = Some("invalid fingerprint".into());
    scan.records.push(std::sync::Arc::new(invalid));
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
    let mut rejected = (*scan.records.pop().unwrap()).clone();
    rejected.fingerprint = Some(blake3::hash(b"rejected").to_hex().to_string());
    rejected.media.title = "Reject insert".into();
    original.media.title = "Queued update must roll back".into();
    scan.records
        .extend([original, rejected].into_iter().map(std::sync::Arc::new));
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
                records: (vec![original.clone()])
                    .into_iter()
                    .map(std::sync::Arc::new)
                    .collect(),
                ..ScanResult::default()
            })
            .unwrap();
        let id = store
            .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
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
            records: records.into_iter().map(std::sync::Arc::new).collect(),
            ..ScanResult::default()
        };
        store.apply_scan(&scan).unwrap();
        let retained = store.track(id).unwrap().unwrap();
        assert_eq!(retained.path, original.path);
        assert!(retained.favorite);
        assert!(retained.missing);
        let first_id = store
            .track_id_for_identity(crate::store::TrackIdentity::File(&first.path))
            .unwrap()
            .unwrap();
        let second_id = store
            .track_id_for_identity(crate::store::TrackIdentity::File(&second.path))
            .unwrap()
            .unwrap();
        assert_ne!(first_id, id);
        assert_ne!(second_id, id);
        assert_ne!(first_id, second_id);
        assert_eq!(store.queue_ids().unwrap(), [queue_id]);
        let groups = store.duplicate_groups_page(0, 10).unwrap();
        assert_eq!(groups.rows[0].missing_members, 1);
        assert_eq!(groups.rows[0].present_members, 2);
        assert!(!store.apply_scan(&scan).unwrap().changed);
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
            records: (vec![first.clone(), second.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    let first_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&first.path))
        .unwrap()
        .unwrap();
    let second_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&second.path))
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
            records: (vec![target.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    let target_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&target.path))
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
            records: (vec![original.clone(), existing.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    let source_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
        .unwrap()
        .unwrap();
    let existing_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&existing.path))
        .unwrap()
        .unwrap();
    original.path = directory.path().join("new/moved.wav");
    store
        .apply_scan(&ScanResult {
            roots: vec![directory.path().join("new")],
            records: (vec![existing.clone(), original.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
            .unwrap(),
        Some(source_id)
    );
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&existing.path))
            .unwrap(),
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
            records: (vec![original.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
        .unwrap()
        .unwrap();
    store.keep_independent(id).unwrap();
    let mut copy = original.clone();
    copy.path = directory.path().join("new/copy.wav");
    store
        .apply_scan(&ScanResult {
            roots: vec![directory.path().join("new")],
            records: (vec![copy.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    assert_ne!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&copy.path))
            .unwrap(),
        Some(id)
    );
    assert_eq!(store.track(id).unwrap().unwrap().path, original.path);
    assert!(store.track(id).unwrap().unwrap().missing);
    store
        .apply_scan(&ScanResult {
            records: (vec![original.clone(), copy])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..ScanResult::default()
        })
        .unwrap();
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
            .unwrap(),
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
#[test]
fn other_check_rechecks_current_membership_and_never_admits_or_relocates() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let original = record();
    let mut inside = record();
    inside.path = "/configured/inside.wav".into();
    store
        .apply_scan(&ScanResult {
            records: (vec![original.clone(), inside.clone()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            ..Default::default()
        })
        .unwrap();
    let original_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
        .unwrap()
        .unwrap();
    let mut unknown = original.clone();
    unknown.path = "/elsewhere/unregistered.wav".into();
    let mut changed = original.clone();
    changed.media.title = "Refreshed".into();
    changed.media.duration = Some(0.25);
    let mut result = ScanResult {
        roots: vec![inside.path.clone()],
        records: (vec![changed, unknown.clone()])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        min_duration_secs: 60,
        ..Default::default()
    };
    store
        .apply_other_tracks_check(&mut result, &["/configured".into()])
        .unwrap();
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
            .unwrap(),
        Some(original_id)
    );
    assert_eq!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&unknown.path))
            .unwrap(),
        None
    );
    assert_eq!(
        store.track(original_id).unwrap().unwrap().title,
        "Refreshed"
    );
    assert_eq!(
        store.track(original_id).unwrap().unwrap().duration,
        Some(0.25)
    );
    let inside_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&inside.path))
        .unwrap()
        .unwrap();
    assert!(!store.track(inside_id).unwrap().unwrap().missing);

    // Roots changed during preparation: neither metadata nor absence from the
    // old Other scope may affect a source that is now configured.
    let mut stale = ScanResult {
        roots: vec![original.path.clone()],
        records: (vec![original.clone()])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        ..Default::default()
    };
    assert!(
        !store
            .apply_other_tracks_check(&mut stale, &["/music".into()])
            .unwrap()
            .changed
    );
    assert_eq!(
        store.track(original_id).unwrap().unwrap().title,
        "Refreshed"
    );
}

#[test]
fn other_check_preserves_registered_cue_identity_and_user_state_across_absence() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let ordinary = std::sync::Arc::new(record());
    let registered_cue = CueSegment {
        sheet: "/sheets/registered.cue".into(),
        number: 7,
        start_frame: 750,
        end_frame: Some(1500),
    };
    let cue = CueRecord::registered(
        ordinary.clone(),
        registered_cue.clone(),
        RegisteredCueMetadata {
            title: "Registered segment".into(),
            artist: "Registered artist".into(),
            album: "Registered album".into(),
            track_number: None,
            disc_number: Some(3),
            release_date: Some("1999-02-03".into()),
        },
        Some(10.0),
    );
    store
        .apply_scan(&ScanResult {
            records: vec![ordinary.clone()],
            segments: vec![cue],
            ..Default::default()
        })
        .unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&ordinary.path))
        .unwrap()
        .unwrap();
    let cue_id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(
            Path::new("/sheets/registered.cue"),
            7,
        ))
        .unwrap()
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE tracks SET title_override='Personal',play_count=9 WHERE id=?1",
            [cue_id],
        )
        .unwrap();
    store.enqueue_tracks(&[cue_id, id, cue_id]).unwrap();
    let occurrences = || {
        store
            .queue_page(None, 0, 10)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| (row.id, row.track_id, row.position))
            .collect::<Vec<_>>()
    };
    let entries = occurrences();
    store.set_queue_current(Some(entries[2].0)).unwrap();
    let queue = store.queue_state().unwrap();
    let playlist = store
        .create_playlist_with_tracks("Retained", &[cue_id, id, cue_id])
        .unwrap();
    let playlist_tracks = store.playlist_track_ids(playlist, 10).unwrap();
    let mut absent = ScanResult {
        roots: vec![ordinary.path.clone()],
        ..Default::default()
    };
    assert!(
        store
            .apply_other_tracks_check(&mut absent, &[])
            .unwrap()
            .queue
    );
    assert!(store.track(id).unwrap().unwrap().missing);
    assert!(store.track(cue_id).unwrap().unwrap().missing);
    assert!(absent.records.is_empty());
    assert!(absent.segments.is_empty());
    let missing_queue = store.queue_state().unwrap();
    assert_eq!(occurrences(), entries);
    assert_eq!(
        (
            missing_queue.total,
            missing_queue.current_id,
            missing_queue.current_position
        ),
        (queue.total, queue.current_id, queue.current_position)
    );
    let mut source = ordinary.as_ref().clone();
    source.size = 4096;
    source.modified_ns = 99;
    source.fingerprint = Some(blake3::hash(b"refreshed source").to_hex().to_string());
    source.media.title = "Source title must not replace CUE title".into();
    source.media.artist = "Source artist".into();
    source.media.album = "Source album".into();
    source.media.track_number = Some(12);
    source.media.disc_number = None;
    source.media.release_date = Some("2042".into());
    source.media.duration = Some(120.0);
    source.media.codec = "flac".into();
    source.media.channels = 6;
    source.media.sample_rate = 96000;
    source.media.bits_per_sample = Some(24);
    source.media.bitrate_bps = Some(2_000_000);
    source.media.bitrate_exact = false;
    let source = std::sync::Arc::new(source);
    let mut restored = ScanResult {
        records: vec![source.clone()],
        ..Default::default()
    };
    assert!(
        store
            .apply_other_tracks_check(&mut restored, &[])
            .unwrap()
            .queue
    );
    let segment = store.track(cue_id).unwrap().unwrap();
    assert!(!segment.missing);
    assert!(!store.track(id).unwrap().unwrap().missing);
    assert_eq!(segment.title, "Personal");
    assert_eq!(segment.duration, Some(10.0));
    assert_eq!(segment.sample_rate, 96000);
    assert_eq!(segment.cue, Some(registered_cue.clone()));
    assert_eq!(segment.artist, "Registered artist");
    assert_eq!(segment.album, "Registered album");
    assert_eq!(segment.track_number, None);
    assert_eq!(segment.disc_number, Some(3));
    assert_eq!(segment.release_date.as_deref(), Some("1999-02-03"));
    assert_eq!(segment.play_count, 9);
    let stored = store
        .stored_scan_files(std::slice::from_ref(&ordinary.path))
        .unwrap();
    assert_eq!(stored.len(), 2);
    let stored = stored.iter().find(|row| row.track_id == cue_id).unwrap();
    assert_eq!(stored.cue, Some(registered_cue));
    assert_eq!(stored.size, source.size);
    assert_eq!(stored.modified_ns, source.modified_ns);
    assert_eq!(stored.fingerprint, source.fingerprint);
    let raw = stored.media.as_ref().unwrap();
    assert_eq!(raw.title, "Registered segment");
    assert_eq!(raw.artist, "Registered artist");
    assert_eq!(raw.album, "Registered album");
    assert_eq!(raw.track_number, None);
    assert_eq!(raw.disc_number, Some(3));
    assert_eq!(raw.release_date.as_deref(), Some("1999-02-03"));
    assert_eq!(raw.duration, Some(10.0));
    assert_eq!(raw.codec, source.media.codec);
    assert_eq!(raw.channels, source.media.channels);
    assert_eq!(raw.sample_rate, source.media.sample_rate);
    assert_eq!(raw.bits_per_sample, source.media.bits_per_sample);
    assert_eq!(raw.bitrate_bps, source.media.bitrate_bps);
    assert_eq!(raw.bitrate_exact, source.media.bitrate_exact);
    let whole = store.track(id).unwrap().unwrap();
    assert_eq!(whole.title, source.media.title);
    assert_eq!(whole.duration, source.media.duration);
    assert!(whole.cue.is_none());
    let restored_queue = store.queue_state().unwrap();
    assert_eq!(occurrences(), entries);
    assert_eq!(
        (
            restored_queue.total,
            restored_queue.current_id,
            restored_queue.current_position
        ),
        (queue.total, queue.current_id, queue.current_position)
    );
    assert_eq!(
        store.playlist_track_ids(playlist, 10).unwrap(),
        playlist_tracks
    );
    assert_eq!(store.track_count().unwrap(), 2);
}

#[test]
fn import_staging_budget_failure_commits_no_tracks() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let scan = ScanResult {
        records: (vec![record()])
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        retained_budget: Some(Default::default()),
        ..Default::default()
    };
    scan.retained_budget
        .as_ref()
        .unwrap()
        .reserve(64 * 1024 * 1024)
        .unwrap();
    assert!(
        store
            .apply_scan(&scan)
            .unwrap_err()
            .to_string()
            .contains("retained data limit")
    );
    assert_eq!(store.track_count().unwrap(), 0);
    assert!(store.conn.is_autocommit());
}

#[test]
fn import_scope_without_successful_records_still_requires_workspace() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let scan = ScanResult {
        roots: vec!["/missing/source.wav".into()],
        retained_budget: Some(Default::default()),
        ..Default::default()
    };
    let budget = scan.retained_budget.as_ref().unwrap();
    budget.reserve(budget.remaining()).unwrap();
    assert!(
        store
            .apply_scan(&scan)
            .unwrap_err()
            .to_string()
            .contains("retained data limit")
    );
    assert_eq!(budget.remaining(), 0);
    assert_eq!(store.track_count().unwrap(), 0);
    assert!(store.conn.is_autocommit());
}

#[test]
fn import_scope_json_escaping_is_budgeted_and_failure_releases_workspace() {
    for escaped in [false, true] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let path = if escaped {
            "\"\\\u{1}".repeat(1024)
        } else {
            "x".repeat(3072)
        };
        let mut scan = ScanResult {
            roots: vec![format!("/{path}").into()],
            records: (vec![record()])
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            retained_budget: Some(Default::default()),
            ..Default::default()
        };
        let budget = scan.retained_budget.as_ref().unwrap().clone();
        budget.reserve(budget.remaining() - 16 * 1024).unwrap();
        let result = store.apply_scan(&scan);
        if escaped {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("retained data limit")
            );
            assert_eq!(store.track_count().unwrap(), 0);
            assert!(store.conn.is_autocommit());
            assert_eq!(budget.remaining(), 16 * 1024);
            scan.roots.clear();
            assert!(store.apply_scan(&scan).unwrap().changed);
        } else {
            assert!(result.unwrap().changed);
        }
        assert_eq!(store.track_count().unwrap(), 1);
        assert_eq!(budget.remaining(), 16 * 1024);
        assert!(store.conn.is_autocommit());
    }
}

#[test]
fn completed_no_change_scan_retains_track_cache_and_hidden_raw_edits_do_not_clear_it() {
    let (store, mut scan, id) = queued(record());
    let original = store.track(id).unwrap().unwrap();
    assert!(store.track_cache.borrow().entries.contains_key(&id));
    let impacts = store.apply_scan(&scan).unwrap();
    assert_eq!(impacts, ScanImpacts::default());
    store.invalidate_scan_caches(impacts, false);
    assert!(store.track_cache.borrow().entries.contains_key(&id));
    store
        .edit_track(id, Some("Personal"), None, None, None)
        .unwrap();
    let overridden = store.track(id).unwrap().unwrap();
    std::sync::Arc::make_mut(&mut scan.records[0]).media.title = "Hidden raw title".into();
    let impacts = store.apply_scan(&scan).unwrap();
    assert!(impacts.changed);
    assert!(!impacts.library);
    store.invalidate_scan_caches(impacts, false);
    assert!(store.track_cache.borrow().entries.contains_key(&id));
    assert_eq!(store.track(id).unwrap().unwrap(), overridden);
    std::sync::Arc::make_mut(&mut scan.records[0]).media.artist = "Visible artist".into();
    let impacts = store.apply_scan(&scan).unwrap();
    store.invalidate_scan_caches(impacts, false);
    assert!(impacts.library);
    assert!(!store.track_cache.borrow().entries.contains_key(&id));
    assert_ne!(store.track(id).unwrap().unwrap().artist, original.artist);
}

#[test]
fn repeated_scan_staging_releases_its_retained_workspace() {
    let (store, mut scan, _) = queued(record());
    scan.retained_budget = Some(Default::default());
    let budget = scan.retained_budget.as_ref().unwrap();
    budget.reserve(64 * 1024 * 1024 - 8192).unwrap();
    for _ in 0..16 {
        assert!(!store.apply_scan(&scan).unwrap().changed);
        assert_eq!(budget.remaining(), 8192);
    }
}
