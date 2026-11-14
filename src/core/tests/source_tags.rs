use super::*;
use rivu_format::tags::{TagEncoding, TagValues};

fn wav(path: &Path) {
    crate::test_support::write_i16_wav(path, 1, 8_000, [123]).unwrap();
}

fn finish(core: &mut Core) -> Ack {
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
            ..Default::default()
        },
    )
    .unwrap();
    core.command(Command::LoadSourceTags {
        track_id: 1,
        encoding: TagEncoding::Declared,
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(ack.is_success(), "{:?}", ack.error);
    let loaded = ack.source_tags.unwrap();
    assert_eq!(loaded.title, "File title");
    assert_eq!(loaded.release_date.as_deref(), Some("2024-03"));
    assert_eq!(core.track(1).unwrap(), before);
    core.command(Command::SyncTrackTags {
        track_id: 1,
        tags: loaded.clone(),
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    let after = core.track(1).unwrap();
    assert_eq!(source_tags::track_tags(&after), loaded);
    assert_eq!(after.play_count, before.play_count);
    assert_eq!(after.favorite, before.favorite);
    assert_eq!(after.decoder_hint, before.decoder_hint);
    assert_eq!(after.cue, before.cue);
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
    };
    core.command(Command::SaveSourceTags {
        track_id: 1,
        tags: requested.clone(),
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(ack.is_success(), "{:?}", ack.error);
    assert!(ack.source_file_saved);
    assert_eq!(ack.source_tags, Some(requested.clone()));
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
    assert!(ack.is_success(), "{:?}", ack.error);
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
fn physical_file_success_is_reported_when_database_write_fails() {
    let (directory, mut core) = fixture();
    let before = core.track(1).unwrap();
    wav(&before.path);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_source_sync BEFORE UPDATE OF title ON tracks BEGIN SELECT RAISE(ABORT,'reject database tags'); END;").unwrap();
    let mut requested = source_tags::track_tags(&before);
    requested.title = "File committed".into();
    core.command(Command::SaveSourceTags {
        track_id: 1,
        tags: requested.clone(),
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(!ack.is_success());
    assert!(ack.source_file_saved);
    assert_eq!(ack.source_tags, Some(requested));
    assert!(
        ack.error
            .unwrap()
            .contains("retry database synchronization only")
    );
    assert_eq!(core.track(1).unwrap().title, before.title);
    let read = rivu_metadata::read_tags(&mut std::fs::File::open(&before.path).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(read.title.as_deref(), Some("File committed"));
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
    assert!(!ack.source_file_saved);
    assert_eq!(core.track(1).unwrap(), before);
}

#[test]
fn failed_source_fact_commit_can_retry_without_rewriting_the_file() {
    let (directory, mut core) = fixture();
    let before = core.track(1).unwrap();
    wav(&before.path);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_facts BEFORE UPDATE OF fingerprint ON tracks BEGIN SELECT RAISE(ABORT,'reject source facts'); END;").unwrap();
    let mut tags = source_tags::track_tags(&before);
    tags.title = "Committed file".into();
    core.command(Command::SaveSourceTags {
        track_id: 1,
        tags: tags.clone(),
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    let ack = finish(&mut core);
    assert!(!ack.is_success());
    assert!(ack.source_file_saved);
    assert!(core.pending_written_source.is_some());
    let saved = std::fs::read(&before.path).unwrap();
    use std::os::unix::fs::MetadataExt;
    let inode = std::fs::metadata(&before.path).unwrap().ino();
    connection
        .execute_batch("DROP TRIGGER reject_facts;")
        .unwrap();
    core.command(Command::SyncTrackTags {
        track_id: 1,
        tags: tags.clone(),
        expected: Box::new(source_tags::track_tags(&before)),
    })
    .unwrap();
    assert!(core.pending_written_source.is_none());
    assert_eq!(source_tags::track_tags(&core.track(1).unwrap()), tags);
    let hash: Vec<u8> = connection
        .query_row("SELECT fingerprint FROM tracks WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(hash, blake3::hash(&saved).as_bytes());
    assert_eq!(std::fs::read(&before.path).unwrap(), saved);
    assert_eq!(std::fs::metadata(&before.path).unwrap().ino(), inode);
}
