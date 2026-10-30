use super::*;

#[test]
fn playback_precision_upgrade_is_source_conditional_and_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("precision.db");
    let store = Store::open(&database).unwrap();
    let mut record = rec("/music/source.flac", "source", "Source", 10.0);
    record.media.bitrate_bps = Some(128_000);
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let track = all_tracks(&store).remove(0);
    store.enqueue_tracks(&[track.id]).unwrap();
    let source = store.playback_source(&track).unwrap().unwrap();
    assert!(!store.upgrade_bitrate(&source, 192_000, false).unwrap());
    assert!(!store.upgrade_bitrate(&source, 0, true).unwrap());
    let before = store.queue_state().unwrap();
    // Precision itself is content, even when the numerical value is unchanged.
    assert!(store.upgrade_bitrate(&source, 128_000, true).unwrap());
    let exact = store.track(track.id).unwrap().unwrap();
    assert!(exact.bitrate_exact);
    assert!(
        store.queue_page(None, 0, 10).unwrap().rows[0]
            .track
            .bitrate_exact
    );
    assert!(
        store
            .library_view_page(
                None,
                None,
                None,
                rivu_protocol::model::LibrarySort::Id,
                0,
                10
            )
            .unwrap()
            .rows[0]
            .bitrate_exact
    );
    assert!(
        store
            .known_source(&record.path)
            .unwrap()
            .unwrap()
            .media
            .unwrap()
            .bitrate_exact
    );
    let revision = store.queue_state().unwrap();
    assert_eq!(revision, before);
    assert!(!store.upgrade_bitrate(&source, 256_000, true).unwrap());
    assert!(!store.upgrade_bitrate(&source, 256_000, false).unwrap());
    assert_eq!(store.queue_state().unwrap(), revision);
    assert!(store.playback_source(&exact).unwrap().is_none());

    // Ordinary and forced probes can both arrive after the playback result.
    for bitrate in [Some(96_000), None] {
        record.media.bitrate_bps = bitrate;
        assert!(
            !store
                .apply_scan(&scan(vec![record.clone()]))
                .unwrap()
                .changed
        );
        assert_eq!(store.track(track.id).unwrap().unwrap(), exact);
        assert_eq!(store.queue_state().unwrap(), revision);
    }
    drop(store);
    let store = Store::open(&database).unwrap();
    assert!(store.track(track.id).unwrap().unwrap().bitrate_exact);
    record.fingerprint = Some(blake3::hash(b"replacement").to_hex().to_string());
    record.modified_ns += 1;
    record.media.bitrate_bps = Some(64_000);
    assert!(
        store
            .apply_scan(&scan(vec![record.clone()]))
            .unwrap()
            .changed
    );
    let replacement = store.track(track.id).unwrap().unwrap();
    assert_eq!(replacement.bitrate_bps, Some(64_000));
    assert!(!replacement.bitrate_exact);
    assert!(!store.upgrade_bitrate(&source, 128_000, true).unwrap());
    record.media.bitrate_exact = true;
    assert!(store.apply_scan(&scan(vec![record])).unwrap().changed);
    assert!(store.track(track.id).unwrap().unwrap().bitrate_exact);
}

#[test]
fn playback_precision_fill_and_cue_guard_preserve_source_identity() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = rec("/music/source.flac", "source", "Source", 10.0);
    record.fingerprint = None;
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let track = all_tracks(&store).remove(0);
    let source = store.playback_source(&track).unwrap().unwrap();
    assert!(store.upgrade_bitrate(&source, 128_000, false).unwrap());
    assert!(!store.upgrade_bitrate(&source, 192_000, false).unwrap());
    assert!(store.upgrade_bitrate(&source, 192_000, true).unwrap());
    assert!(
        !store
            .apply_scan(&scan(vec![record.clone()]))
            .unwrap()
            .changed
    );
    record.modified_ns += 1;
    store.apply_scan(&scan(vec![record])).unwrap();
    assert_eq!(store.track(track.id).unwrap().unwrap().bitrate_bps, None);
    assert!(!store.upgrade_bitrate(&source, 192_000, true).unwrap());

    let mut cue = segment("/music/album.cue", 1, 0, Some(750));
    cue.source = std::sync::Arc::new(rec("/music/cue.flac", "cue", "Cue", 10.0));
    cue.duration = Some(10.0);
    store
        .apply_scan(&ScanResult {
            segments: vec![cue.clone()],
            ..scan(Vec::new())
        })
        .unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(
            Path::new("/music/album.cue"),
            1,
        ))
        .unwrap()
        .unwrap();
    let source = store
        .playback_source(&store.track(id).unwrap().unwrap())
        .unwrap()
        .unwrap();
    assert!(store.upgrade_bitrate(&source, 128_000, true).unwrap());
    let mut changed = segment("/music/album.cue", 1, 0, Some(700));
    changed.source = cue.source;
    changed.duration = cue.duration;
    store
        .apply_scan(&ScanResult {
            segments: vec![changed],
            ..scan(Vec::new())
        })
        .unwrap();
    assert!(!store.track(id).unwrap().unwrap().bitrate_exact);
    assert!(!store.upgrade_bitrate(&source, 192_000, true).unwrap());
}

#[test]
fn decoder_hint_persists_without_overwriting_track_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("decoder-hint.db");
    let store = Store::open(&path).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/native.wav", "native", "Native", 10.),
            rec("/music/ffmpeg.wav", "ffmpeg", "FFmpeg", 20.),
        ]))
        .unwrap();
    let mut expected = all_tracks(&store);
    assert!(
        expected
            .iter()
            .all(|track| track.decoder_hint == DecoderHint::Auto)
    );
    let native = expected[0].id;
    let ffmpeg = expected[1].id;
    store.set_favorite(&[native], true).unwrap();
    store
        .edit_track(native, Some("Custom"), None, None, None)
        .unwrap();
    store.mark_played(native, 42).unwrap();
    store.increment_play_count(native).unwrap();
    expected[0] = store.track(native).unwrap().unwrap(); // populate the cache
    store.enqueue_tracks(&[native, native, ffmpeg]).unwrap();
    let queue_before_hint = store.queue_state().unwrap();
    let page_before_hint = store.queue_page(None, 0, 10).unwrap();
    store
        .edit_track(native, None, None, None, Some(DecoderHint::Native))
        .unwrap();
    store
        .edit_track(ffmpeg, None, None, None, Some(DecoderHint::Ffmpeg))
        .unwrap();
    expected[0].decoder_hint = DecoderHint::Native;
    expected[1].decoder_hint = DecoderHint::Ffmpeg;
    assert_eq!(store.queue_state().unwrap(), queue_before_hint);
    assert_eq!(store.queue_page(None, 0, 10).unwrap(), page_before_hint);
    assert!(
        !store
            .edit_track(native, None, None, None, Some(DecoderHint::Native))
            .unwrap()
            .changed
    );
    assert_eq!(store.track(native).unwrap().unwrap(), expected[0]);
    assert!(
        store
            .edit_track(999, Some("Missing"), None, None, Some(DecoderHint::Native))
            .is_err()
    );
    assert_eq!(all_tracks(&store), expected);
    assert!(
        store
            .conn
            .execute("UPDATE tracks SET decoder_hint=3 WHERE id=?1", [native])
            .is_err()
    );
    assert!(
        store
            .conn
            .execute("UPDATE tracks SET decoder_hint=NULL WHERE id=?1", [native])
            .is_err()
    );
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(all_tracks(&reopened), expected);
    reopened
        .edit_track(native, None, None, None, Some(DecoderHint::Auto))
        .unwrap();
    assert_eq!(
        reopened.track(native).unwrap().unwrap().decoder_hint,
        DecoderHint::Auto
    );
}

#[test]
fn metadata_patch_omitting_hint_preserves_concurrent_preference() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata-patch.db");
    let store = Store::open(&path).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.wav", "a", "Raw", 10.)]))
        .unwrap();
    let baseline = all_tracks(&store)[0].clone();
    let concurrent = Store::open(&path).unwrap();
    concurrent
        .edit_track(baseline.id, None, None, None, Some(DecoderHint::Ffmpeg))
        .unwrap();
    store
        .edit_track(baseline.id, Some("Edited"), None, None, None)
        .unwrap();
    let saved = store.track(baseline.id).unwrap().unwrap();
    assert_eq!(saved.title, "Edited");
    assert_eq!(saved.decoder_hint, DecoderHint::Ffmpeg);
    assert_eq!(saved.artist, baseline.artist);
    assert_eq!(saved.album, baseline.album);
}

#[test]
fn combined_metadata_patch_commits_or_fails_as_one_update() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.wav", "a", "Raw", 10.)]))
        .unwrap();
    let id = all_tracks(&store)[0].id;
    store.set_favorite(&[id], true).unwrap();
    store.mark_played(id, 42).unwrap();
    store.increment_play_count(id).unwrap();
    let before = store.track(id).unwrap().unwrap();
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER reject_decoder_hint BEFORE UPDATE OF decoder_hint ON tracks
         WHEN NEW.decoder_hint=2 BEGIN SELECT RAISE(ABORT,'rejected hint'); END;",
        )
        .unwrap();
    assert!(
        store
            .edit_track(
                id,
                Some("Edited"),
                Some("Artist"),
                Some("Album"),
                Some(DecoderHint::Ffmpeg)
            )
            .is_err()
    );
    assert_eq!(store.track(id).unwrap().unwrap(), before);
    store
        .conn
        .execute_batch("DROP TRIGGER reject_decoder_hint")
        .unwrap();
    store
        .edit_track(
            id,
            Some("Edited"),
            Some("Artist"),
            Some("Album"),
            Some(DecoderHint::Ffmpeg),
        )
        .unwrap();
    let mut expected = before;
    expected.title = "Edited".into();
    expected.artist = "Artist".into();
    expected.album = "Album".into();
    expected.decoder_hint = DecoderHint::Ffmpeg;
    assert_eq!(store.track(id).unwrap().unwrap(), expected);
    assert!(
        store
            .edit_track(
                id + 1,
                Some("Missing"),
                Some("Artist"),
                Some("Album"),
                Some(DecoderHint::Native)
            )
            .is_err()
    );
    assert_eq!(all_tracks(&store), vec![expected]);
}

#[test]
fn recent_tracks_merge_replays_without_losing_counts() {
    let (directory, s) = db();
    let p = directory.path().join("library.db");
    s.apply_scan(&scan(vec![
        rec("/music/a.mp3", "a", "A", 600.),
        rec("/music/b.mp3", "b", "B", 100.),
    ]))
    .unwrap();
    let tracks = all_tracks(&s);
    let (a, b) = (tracks[0].id, tracks[1].id);
    s.mark_played(a, 10).unwrap();
    s.increment_play_count(a).unwrap();
    s.mark_played(b, 20).unwrap();
    s.mark_played(a, 30).unwrap();
    s.increment_play_count(a).unwrap();
    drop(s);
    let s = Store::open(&p).unwrap();
    assert_eq!(all_tracks(&s)[0].play_count, 2);
    assert_eq!(all_tracks(&s)[1].play_count, 0);
    assert_eq!(
        s.history(10)
            .unwrap()
            .iter()
            .map(|entry| (entry.track_id, entry.played_at))
            .collect::<Vec<_>>(),
        vec![(a, 30), (b, 20)]
    );
    s.remove_tracks(&[a]).unwrap();
    assert_eq!(
        s.history(10)
            .unwrap()
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        vec![b]
    );
}

#[test]
fn title_patch_keeps_unedited_metadata_live_across_rescans() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = rec("/music/a.mp3", "same", "Raw title", 100.);
    record.media.artist = "Raw artist".into();
    record.media.album = "Raw album".into();
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let id = all_tracks(&store)[0].id;
    // Prime the cache before editing and scanning.
    assert_eq!(store.track(id).unwrap().unwrap().artist, "Raw artist");
    store
        .edit_track(id, Some("Edited title"), None, None, None)
        .unwrap();
    record.media.title = "Rescanned title".into();
    record.media.artist = "Rescanned artist".into();
    record.media.album = "Rescanned album".into();
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.title, "Edited title");
    assert_eq!(track.artist, "Rescanned artist");
    assert_eq!(track.album, "Rescanned album");

    // Empty text is an actual override, unlike an omitted field.
    store.edit_track(id, None, Some(""), None, None).unwrap();
    record.media.artist = "Another artist".into();
    record.media.album = "Another album".into();
    store.apply_scan(&scan(vec![record])).unwrap();
    let track = store.track(id).unwrap().unwrap();
    assert_eq!(track.title, "Edited title");
    assert_eq!(track.artist, "");
    assert_eq!(track.album, "Another album");
}

#[test]
fn metadata_refresh_preserves_favorites_and_unknown_values() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = rec("/music/tagged.wav", "hash", "A", 10.);
    record.media.bitrate_bps = Some(1_536_000);
    record.media.bits_per_sample = Some(16);
    record.media.track_number = Some(2);
    record.media.disc_number = Some(1);
    record.media.release_date = Some("2024-03".into());
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let id = all_tracks(&store)[0].id;
    store.set_favorite(&[id, id], true).unwrap();
    store.mark_played(id, 42).unwrap();
    store.increment_play_count(id).unwrap();
    record.media.track_number = Some(3);
    record.media.bitrate_bps = None;
    record.fingerprint = Some(blake3::hash(b"replacement").to_hex().to_string());
    store.apply_scan(&scan(vec![record])).unwrap();
    let track = all_tracks(&store).remove(0);
    assert!(track.favorite);
    assert_eq!(
        (track.track_number, track.disc_number, track.bits_per_sample),
        (Some(3), Some(1), Some(16))
    );
    assert_eq!(track.release_date.as_deref(), Some("2024-03"));
    assert_eq!(track.bitrate_bps, None);
    assert_eq!((track.play_count, track.last_played), (1, Some(42)));
    assert!(store.set_favorite(&[id, 999], false).is_err());
    assert!(all_tracks(&store)[0].favorite);
    assert_eq!(
        store.stored_scan_files(&[PathBuf::from("/")]).unwrap()[0]
            .media
            .as_ref()
            .unwrap()
            .track_number,
        Some(3)
    );
}
