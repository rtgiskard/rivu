use super::*;
use rivu_format::tags::{TagEncoding, TagValues};

fn wav(path: &Path) {
    rivu_audio::test_support::write_i16_wav(path, 1, 8_000, [123]).unwrap();
}

fn finish(core: &mut Core) -> CommandReceipt {
    let (reply, response) = oneshot::channel();
    core.active_source_tags.as_mut().unwrap().reply = Some(reply);
    core.shutdown_source_tags();
    match futures::executor::block_on(response).unwrap() {
        CoreResponse::Ack(ack) => ack,
        _ => panic!("source tag operation returned a query response"),
    }
}

#[test]
fn explicit_file_load_does_not_persist_and_bulk_sync_keeps_application_state() {
    let (_directory, mut core) = fixture();
    let before = core.track(1).unwrap();
    wav(&before.path);
    rivu_metadata::edit_file(
        &before.path,
        &rivu_metadata::TagPatch {
            title: Some("File title".into()),
            release_date: Some("2024-03".into()),
            track_number: Some(Some(3)),
            disc_number: Some(Some(2)),
            replaygain_track_gain: Some(Some(-4.5)),
            ..Default::default()
        },
    )
    .unwrap();
    let (reply, mut response) = oneshot::channel();
    core.dispatch_request(Request {
        operation: Operation::Command(Command::LoadSourceTags {
            track_id: 1,
            encoding: TagEncoding::Declared,
        }),
        reply: Some(reply),
    });
    core.ensure_source_tag_scan_allowed().unwrap();
    assert!(response.try_recv().unwrap().is_none());
    let (_sender, requests) = mpsc::bounded(1);
    drive_until(&mut core, requests, |event| {
        matches!(event, dispatch::BusinessEvent::SourceTags(_))
    });
    let Some(CoreResponse::Ack(ack)) = response.try_recv().unwrap() else {
        panic!("expected source tag reply after owner dispatch");
    };
    assert!(ack.is_success(), "{:?}", ack.error());
    let loaded = ack
        .outcome
        .source_tags()
        .and_then(|source| source.tags.as_ref())
        .unwrap();
    assert_eq!(loaded.title, "File title");
    assert_eq!(loaded.release_date.as_deref(), Some("2024-03"));
    assert_eq!(loaded.replaygain_track_gain, Some(-4.5));
    assert_eq!(core.track(1).unwrap(), before);
    core.command(Command::SyncTrackTags {
        track_id: 1,
        tags: loaded.clone(),
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    let after = core.track(1).unwrap();
    assert_eq!(&source_tags::track_tags(&after), loaded);
    assert_eq!(after.play_count, before.play_count);
    assert_eq!(after.favorite, before.favorite);
    assert_eq!(after.decoder_hint, before.decoder_hint);
    assert_eq!(after.cue, before.cue);
}

#[test]
fn bulk_sync_rejects_nonfinite_replaygain_without_changing_any_field() {
    let (_directory, mut core) = fixture();
    let before = core.track(1).unwrap();
    let expected = source_tags::track_tags(&before);
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut tags = expected.clone();
        tags.title = "Must not persist".into();
        tags.replaygain_track_gain = Some(invalid);
        assert!(
            core.command(Command::SyncTrackTags {
                track_id: 1,
                tags,
                expected: Box::new(expected.clone()),
            })
            .is_err()
        );
        assert_eq!(core.track(1).unwrap(), before);
    }
}

#[test]
fn bulk_sync_preflights_live_replaygain_before_committing() {
    let (_directory, mut core) = fixture();
    playing(&mut core, 1);
    let before = core.track(1).unwrap();
    let expected = source_tags::track_tags(&before);
    let revisions = core.state.query_revisions();
    let generation = core.transport.generation;
    let mut tags = expected.clone();
    tags.replaygain_track_gain = Some(-6.0);
    let sync = || Command::SyncTrackTags {
        track_id: 1,
        tags: tags.clone(),
        expected: Box::new(expected.clone()),
    };
    let (audio, _idle_commands) = prefilled_audio(&mut core, 1, 1);

    assert!(core.command(sync()).is_err());
    assert_eq!(core.track(1).unwrap(), before);
    assert_eq!(core.state.query_revisions(), revisions);
    assert_eq!(
        core.state.current_track().unwrap().replaygain_track_gain,
        before.replaygain_track_gain
    );
    assert!(matches!(
        audio.try_recv().unwrap().command,
        AudioCommand::Volume(_)
    ));

    core.command(sync()).unwrap();
    assert_eq!(core.track(1).unwrap().replaygain_track_gain, Some(-6.0));
    assert_eq!(
        core.state.current_track().unwrap().replaygain_track_gain,
        Some(-6.0)
    );
    assert!(matches!(
        audio.try_recv().unwrap().command,
        AudioCommand::Metadata { generation: actual, metadata }
            if actual == generation && metadata.replaygain_track_gain == Some(-6.0)
    ));
}

#[test]
fn stale_bulk_database_baseline_does_not_overwrite_a_concurrent_field_edit() {
    let (_directory, mut core) = fixture();
    let before = source_tags::track_tags(&core.track(1).unwrap());
    core.edit_track(1, Some("Concurrent title".into()), None, None, None, None)
        .unwrap();
    let mut requested = before.clone();
    requested.artist = "Stale artist".into();
    assert!(
        core.command(Command::SyncTrackTags {
            track_id: 1,
            tags: requested,
            expected: Box::new(before)
        })
        .is_err()
    );
    let after = core.track(1).unwrap();
    assert_eq!(after.title, "Concurrent title");
    assert!(after.artist.is_empty());
}

#[test]
fn file_save_commits_verified_dates_and_numbers_without_backups() {
    let (directory, mut core) = fixture();
    let before = core.track(1).unwrap();
    wav(&before.path);
    let requested = TagValues {
        title: "Saved title".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        release_date: Some("2024-03-09".into()),
        track_number: Some(7),
        disc_number: Some(2),
        replaygain_track_gain: Some(-4.5),
    };
    core.command(Command::SaveSourceTags {
        track_id: 1,
        tags: requested.clone(),
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(ack.is_success(), "{:?}", ack.error());
    assert!(
        ack.outcome
            .source_tags()
            .is_some_and(|source| source.file_saved)
    );
    assert_eq!(
        ack.outcome
            .source_tags()
            .and_then(|source| source.tags.as_ref()),
        Some(&requested)
    );
    assert_eq!(source_tags::track_tags(&core.track(1).unwrap()), requested);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    let (hash, size, modified): (Vec<u8>, i64, i64) = connection
        .query_row(
            "SELECT fingerprint,file_size,modified_ns FROM tracks WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let bytes = std::fs::read(&before.path).unwrap();
    assert_eq!(hash, blake3::hash(&bytes).as_bytes());
    assert_eq!(size, bytes.len() as i64);
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(&before.path).unwrap();
    assert_eq!(
        modified,
        metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec()
    );
    let read = rivu_metadata::read_tags(&mut std::fs::File::open(&before.path).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(read.title.as_deref(), Some("Saved title"));
    assert_eq!(read.release_date.as_deref(), Some("2024-03-09"));
    assert_eq!(read.track_number, Some(7));
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".rivu-")
    }));
}

#[test]
fn file_save_updates_shared_cue_source_but_not_equal_content_at_another_path() {
    let (directory, mut core) = fixture();
    let db = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    db.execute_batch(
        "UPDATE tracks SET fingerprint=zeroblob(32);
         UPDATE tracks SET path=(SELECT path FROM tracks WHERE id=1),cue_sheet='a.cue',
             cue_number=1,cue_start_frame=75,cue_end_frame=150 WHERE id=2;
         UPDATE tracks SET path=(SELECT path FROM tracks WHERE id=3),cue_sheet='b.cue',
             cue_number=1,cue_start_frame=75,cue_end_frame=150 WHERE id=4;",
    )
    .unwrap();
    core.store.invalidate_caches();
    let before = core.track(1).unwrap();
    let segment = core.track(2).unwrap();
    let copy = core.track(3).unwrap();
    let copy_segment = core.track(4).unwrap();
    core.state.queue.current_id = core.store.queued_track(2).unwrap();
    core.state.current_track = Some(Arc::new(segment.clone()));
    wav(&before.path);
    let expected = source_tags::track_tags(&before);
    let mut tags = expected.clone();
    tags.title = "New source title".into();
    core.command(Command::SaveSourceTags {
        track_id: 1,
        tags,
        expected: Box::new(expected),
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(ack.is_success(), "{:?}", ack.error());
    let updated = core.track(2).unwrap();
    assert_ne!(updated.fingerprint, segment.fingerprint);
    assert_eq!(updated.fingerprint, core.track(1).unwrap().fingerprint);
    assert_eq!(updated.cue, segment.cue);
    assert_eq!(
        source_tags::track_tags(&updated),
        source_tags::track_tags(&segment)
    );
    assert_eq!(core.state.current_track(), Some(&updated));
    // A warm copy must not hide an accidental write to the other path.
    core.store.invalidate_caches();
    assert_eq!(core.track(3).unwrap(), copy);
    assert_eq!(core.track(4).unwrap(), copy_segment);
    let matching: i64 = db
        .query_row(
            "SELECT count(*) FROM tracks a JOIN tracks b ON a.id=1 AND b.id=2
         WHERE a.file_size=b.file_size AND a.modified_ns=b.modified_ns AND b.probe_version=0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(matching, 1);
}

#[test]
fn saving_unchanged_database_tags_refreshes_stopped_source_identity() {
    let (_directory, mut core) = fixture();
    let before = core.track(1).unwrap();
    wav(&before.path);
    core.state.queue.current_id = core.store.queued_track(1).unwrap();
    core.state.current_track = Some(Arc::new(before.clone()));
    let tags = source_tags::track_tags(&before);
    let revision = core.state.library.revision;
    core.command(Command::SaveSourceTags {
        track_id: 1,
        tags: tags.clone(),
        expected: Box::new(tags.clone()),
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(ack.is_success(), "{:?}", ack.error());
    let stored = core.track(1).unwrap();
    assert_eq!(source_tags::track_tags(&stored), tags);
    let bytes = std::fs::read(&before.path).unwrap();
    let fingerprint = blake3::hash(&bytes).to_hex().to_string();
    assert_eq!(stored.fingerprint.as_deref(), Some(fingerprint.as_str()));
    assert_eq!(
        core.state.current_track.as_ref().unwrap().fingerprint,
        stored.fingerprint
    );
    assert_ne!(core.state.library.revision, revision);
}

#[test]
fn failed_file_write_does_not_commit_displayed_values_to_database() {
    let (_directory, mut core) = fixture();
    let before = core.track(1).unwrap();
    let mut requested = source_tags::track_tags(&before);
    requested.title = "Not saved".into();
    core.command(Command::SaveSourceTags {
        track_id: 1,
        tags: requested,
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(!ack.is_success());
    assert!(
        !ack.outcome
            .source_tags()
            .is_some_and(|source| source.file_saved)
    );
    assert_eq!(core.track(1).unwrap(), before);
}

#[test]
fn either_database_write_can_fail_and_only_the_verified_save_can_recover() {
    use std::os::unix::fs::MetadataExt;

    for column in ["fingerprint", "title"] {
        let (directory, mut core) = fixture();
        let before = core.track(1).unwrap();
        let other = core.track(2).unwrap();
        wav(&before.path);
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER reject_sync BEFORE UPDATE OF {column} ON tracks
                 WHEN OLD.id=1 BEGIN SELECT RAISE(ABORT,'reject database sync'); END;"
            ))
            .unwrap();
        let expected = source_tags::track_tags(&before);
        let tags = TagValues {
            title: "Verified file title".into(),
            artist: "Verified artist".into(),
            track_number: Some(7),
            ..expected.clone()
        };
        core.command(Command::SaveSourceTags {
            track_id: 1,
            tags: tags.clone(),
            expected: Box::new(expected.clone()),
        })
        .unwrap();
        assert!(core.ensure_source_tag_scan_allowed().is_err());
        let ack = finish(&mut core);
        assert!(core.ensure_source_tag_scan_allowed().is_err());
        let recovery = core
            .shared
            .read()
            .system
            .source_tag_recovery
            .clone()
            .unwrap();
        assert_eq!(recovery.track_id, 1);
        assert_eq!(recovery.tags, tags);
        assert!(recovery.error.contains("reject database sync"));
        assert!(!ack.is_success(), "{column}");
        assert!(
            ack.outcome
                .source_tags()
                .is_some_and(|source| source.file_saved)
        );
        assert_eq!(
            ack.outcome
                .source_tags()
                .and_then(|source| source.tags.as_ref()),
            Some(&tags)
        );
        assert!(
            ack.outcome
                .source_tags()
                .and_then(|source| source.refresh_error.as_deref())
                .is_none()
        );
        assert!(
            ack.error()
                .unwrap()
                .contains("retry database synchronization only")
        );
        let saved = std::fs::read(&before.path).unwrap();
        let metadata = std::fs::metadata(&before.path).unwrap();
        let persisted = core.track(1).unwrap();
        assert_eq!(source_tags::track_tags(&persisted), expected);
        let fingerprint = blake3::hash(&saved).to_hex().to_string();
        assert_eq!(
            persisted.fingerprint.as_deref(),
            (column == "title").then_some(fingerprint.as_str())
        );
        let read = rivu_metadata::read_tags(&mut std::fs::File::open(&before.path).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(read.title.as_deref(), Some(tags.title.as_str()));

        assert!(operation_ack(&mut core, Operation::Command(Command::DismissError)).is_success());
        let snapshot = core
            .shared
            .read()
            .system
            .source_tag_recovery
            .clone()
            .unwrap();
        assert_eq!(snapshot, recovery);
        let retry = || Command::RetrySourceTagSync {
            track_id: snapshot.track_id,
            tags: snapshot.tags.clone(),
        };
        assert!(
            !operation_ack(&mut core, Operation::Command(retry())).is_success(),
            "an unsuccessful retry must stay pending"
        );
        connection
            .execute_batch("DROP TRIGGER reject_sync;")
            .unwrap();
        for command in [
            Command::SaveSourceTags {
                track_id: 1,
                tags: expected.clone(),
                expected: Box::new(expected.clone()),
            },
            Command::SyncTrackTags {
                track_id: 1,
                tags: expected.clone(),
                expected: Box::new(expected.clone()),
            },
            Command::RetrySourceTagSync {
                track_id: 1,
                tags: expected,
            },
            Command::RetrySourceTagSync {
                track_id: 2,
                tags: tags.clone(),
            },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(core.track(1).unwrap(), persisted);
            assert_eq!(core.track(2).unwrap(), other);
        }
        let mut other_tags = source_tags::track_tags(&other);
        other_tags.title = "Ordinary database-only edit".into();
        core.command(Command::SyncTrackTags {
            track_id: 2,
            tags: other_tags.clone(),
            expected: Box::new(source_tags::track_tags(&other)),
        })
        .unwrap();

        let ack = operation_ack(&mut core, Operation::Command(retry()));
        assert!(ack.is_success(), "{:?}", ack.error());
        assert!(
            !ack.outcome
                .source_tags()
                .is_some_and(|source| source.file_saved)
        );
        assert_eq!(
            ack.outcome
                .source_tags()
                .and_then(|source| source.tags.as_ref()),
            Some(&tags)
        );
        let recovered = core.track(1).unwrap();
        assert_eq!(source_tags::track_tags(&recovered), tags);
        assert_eq!(recovered.fingerprint.as_deref(), Some(fingerprint.as_str()));
        assert_eq!(source_tags::track_tags(&core.track(2).unwrap()), other_tags);
        assert_eq!(std::fs::read(&before.path).unwrap(), saved);
        assert_eq!(
            std::fs::metadata(&before.path).unwrap().ino(),
            metadata.ino()
        );
        assert!(core.pending_written_source.is_none());
        assert!(core.shared.read().system.source_tag_recovery.is_none());
        assert!(core.shared.read().system.last_error.is_none());
        core.ensure_source_tag_scan_allowed().unwrap();
        assert!(
            core.command(retry()).is_err(),
            "retry must not become ordinary Sync"
        );
    }
}

#[test]
fn postcommit_refresh_failure_is_not_a_retryable_database_failure() {
    for recover_first in [false, true] {
        let (directory, mut core) = fixture();
        let before = core.track(1).unwrap();
        wav(&before.path);
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        if recover_first {
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_tags BEFORE UPDATE OF title ON tracks
                     BEGIN SELECT RAISE(ABORT,'reject database tags'); END;",
                )
                .unwrap();
        }
        core.state.current_track = Some(Arc::new(before.clone()));
        // The out-of-range queue ID fails projection refresh, not either track write.
        core.state.queue.current_id = Some(u64::MAX);
        let expected = source_tags::track_tags(&before);
        let mut tags = expected.clone();
        tags.title = "Committed before refresh".into();
        core.command(Command::SaveSourceTags {
            track_id: 1,
            tags: tags.clone(),
            expected: Box::new(expected),
        })
        .unwrap();
        let mut ack = finish(&mut core);
        if recover_first {
            assert!(!ack.is_success());
            assert!(
                ack.outcome
                    .source_tags()
                    .and_then(|source| source.refresh_error.as_deref())
                    .is_none()
            );
            connection
                .execute_batch("DROP TRIGGER reject_tags;")
                .unwrap();
            ack = operation_ack(
                &mut core,
                Operation::Command(Command::RetrySourceTagSync {
                    track_id: 1,
                    tags: tags.clone(),
                }),
            );
        }
        assert!(ack.is_success(), "{:?}", ack.error());
        assert_eq!(
            ack.outcome
                .source_tags()
                .is_some_and(|source| source.file_saved),
            !recover_first
        );
        assert_eq!(
            ack.outcome
                .source_tags()
                .and_then(|source| source.tags.as_ref()),
            Some(&tags)
        );
        assert!(
            ack.outcome
                .source_tags()
                .and_then(|source| source.refresh_error.as_deref())
                .unwrap()
                .contains("File and database saved; refresh failed")
        );
        assert_eq!(source_tags::track_tags(&core.track(1).unwrap()), tags);
        assert!(core.pending_written_source.is_none());
        assert!(core.shared.read().system.source_tag_recovery.is_none());
        assert!(core.projections.current_track);
    }
}
