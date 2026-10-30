use super::*;

#[test]
fn playlist_preserves_occurrences_across_edits_and_restart() {
    let (directory, s) = db();
    let p = directory.path().join("library.db");
    s.apply_scan(&scan(vec![
        rec("/music/a.mp3", "a", "A", 100.),
        rec("/music/b.mp3", "b", "B", 100.),
        rec("/music/c.mp3", "c", "C", 100.),
    ]))
    .unwrap();
    let ids: Vec<_> = all_tracks(&s).iter().map(|track| track.id).collect();
    let pl = s.create_playlist("mix").unwrap();
    s.add_playlist(pl, &[ids[0], ids[1], ids[0]]).unwrap();
    let first_entry = playlist_entries(&s, pl)[0].id;
    s.add_playlist(pl, &[ids[1], ids[2], ids[2]]).unwrap();
    let entries = playlist_entries(&s, pl);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        vec![ids[0], ids[1], ids[0], ids[1], ids[2], ids[2]]
    );
    assert_eq!(entries[0].id, first_entry);
    assert_eq!(s.move_playlist_entry(entries[4].id, 0).unwrap(), Some(pl));
    s.remove_playlist_entry(first_entry).unwrap();
    drop(s);
    assert_eq!(
        playlist_entries(&Store::open(&p).unwrap(), pl)
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        vec![ids[2], ids[1], ids[0], ids[1], ids[2]]
    );
}

#[test]
fn deleting_tracks_keeps_playlist_reordering_contiguous() {
    let (_directory, store) = db();
    store
        .apply_scan(&scan(vec![
            rec("/music/a.wav", "a", "A", 100.0),
            rec("/music/b.wav", "b", "B", 100.0),
            rec("/music/c.wav", "c", "C", 100.0),
        ]))
        .unwrap();
    let tracks = all_tracks(&store);
    let playlist = store.create_playlist("mix").unwrap();
    store
        .add_playlist(
            playlist,
            &[
                tracks[0].id,
                tracks[1].id,
                tracks[0].id,
                tracks[2].id,
                tracks[1].id,
            ],
        )
        .unwrap();
    let before = playlist_entries(&store, playlist);
    store.remove_tracks(&[tracks[0].id]).unwrap();
    assert_eq!(
        store.move_playlist_entry(before[3].id, 0).unwrap(),
        Some(playlist)
    );
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![before[3].id, before[1].id, before[4].id]
    );
}

#[test]
fn playlist_name_conflicts_preserve_catalog_entries_and_queue() {
    let (_directory, store) = search_store();
    let (mix, _) = store.create_playlist_from_queue("Mix").unwrap();
    let other = store.create_playlist("Other").unwrap();
    let catalog = store.playlist_summary_page(None, 0, 20).unwrap();
    let entries = playlist_entries(&store, mix);
    let queue = store.queue_page(None, 0, 20).unwrap();

    assert!(store.create_playlist("Mix").is_err());
    assert!(store.create_playlist_with_tracks("Mix", &[1]).is_err());
    assert!(store.create_playlist_from_queue("Mix").is_err());
    assert!(store.rename_playlist(other, "Mix").is_err());
    assert!(!store.rename_playlist(mix, "Mix").unwrap());
    assert_eq!(store.playlist_summary_page(None, 0, 20).unwrap(), catalog);
    assert_eq!(playlist_entries(&store, mix), entries);
    assert_eq!(store.queue_page(None, 0, 20).unwrap(), queue);

    store.create_playlist("mix").unwrap();
    store.create_playlist(" Mix ").unwrap();
    assert!(store.rename_playlist(mix, "Renamed").unwrap());
    let reused = store.create_playlist("Mix").unwrap();
    store.delete_playlist(reused).unwrap();
    assert!(store.rename_playlist(other, "Mix").unwrap());
}

#[test]
fn playlist_addition_with_missing_track_rolls_back_new_entries() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![
            rec("/music/a.wav", "a", "A", 10.),
            rec("/music/b.wav", "b", "B", 10.),
        ]))
        .unwrap();
    let tracks = all_tracks(&store);
    let playlist = store.create_playlist("mix").unwrap();
    store.add_playlist(playlist, &[tracks[0].id]).unwrap();
    assert!(
        store
            .add_playlist(playlist, &[tracks[0].id, tracks[1].id, 999])
            .is_err()
    );
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        vec![tracks[0].id]
    );
}

#[test]
fn playlist_pages_and_batches_preserve_effective_audio_metadata() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut whole = rec("/music/shared.wav", "shared", "Whole", 180.0);
    whole.media.codec = "pcm".into();
    let mut bounded = segment("/music/a.cue", 1, 1500, Some(3750));
    bounded.duration = Some(30.0);
    std::sync::Arc::make_mut(&mut bounded.source).media.codec = "pcm".into();
    let mut tail = segment("/music/a.cue", 2, 3750, None);
    tail.duration = Some(130.0);
    let tail_media = &mut std::sync::Arc::make_mut(&mut tail.source).media;
    tail_media.codec = "pcm".into();
    tail_media.channels = 6;
    tail_media.sample_rate = 96_000;
    tail_media.bits_per_sample = Some(24);
    tail_media.bitrate_bps = Some(13_824_000);
    tail_media.bitrate_exact = true;
    let mut unknown = rec("/music/unknown.opus", "unknown", "Unknown", 1.0);
    unknown.media.duration = None;
    unknown.media.codec = "opus".into();
    store
        .apply_scan(&ScanResult {
            segments: vec![bounded, tail],
            ..scan(vec![whole, unknown])
        })
        .unwrap();
    let tracks = all_tracks(&store);
    let ids: Vec<_> = ["Track 2", "Unknown", "Track 1", "Whole"]
        .iter()
        .map(|title| {
            tracks
                .iter()
                .find(|track| track.title == *title)
                .unwrap()
                .id
        })
        .collect();
    store
        .conn
        .execute(
            "UPDATE tracks SET favorite=1,missing=1 WHERE id=?1",
            [ids[2]],
        )
        .unwrap();
    let playlist = store.create_playlist_with_tracks("metadata", &ids).unwrap();
    let expected = [
        (Some(130.0), "pcm"),
        (None, "opus"),
        (Some(30.0), "pcm"),
        (Some(180.0), "pcm"),
    ];
    for query in [None, Some("Track")] {
        let indices: &[usize] = if query.is_some() {
            &[0, 2]
        } else {
            &[0, 1, 2, 3]
        };
        let mut after = None;
        for (offset, &position) in indices.iter().enumerate() {
            let page = store
                .playlist_entries_page(playlist, query, offset, 1)
                .unwrap();
            assert_eq!(page.total, indices.len());
            assert_eq!(page.rows.len(), 1);
            let row = &page.rows[0];
            assert_eq!(row.track_id, ids[position]);
            assert_eq!(row.position, position);
            assert_eq!(row.offset, offset);
            assert_eq!((row.duration, row.codec.as_str()), expected[position]);
            assert_eq!(row.favorite, position == 2);
            assert_eq!(row.missing, position == 2);
            let source = tracks
                .iter()
                .find(|track| track.id == row.track_id)
                .unwrap();
            assert_eq!(row.path, source.path);
            assert_eq!(row.channels, source.channels);
            assert_eq!(row.sample_rate, source.sample_rate);
            assert_eq!(row.bits_per_sample, source.bits_per_sample);
            assert_eq!(row.bitrate_bps, source.bitrate_bps);
            assert_eq!(row.bitrate_exact, source.bitrate_exact);
            assert_eq!(row.cue_number, source.cue.as_ref().map(|cue| cue.number));
            let batch = store
                .playlist_entry_batch(playlist, query, after, 1, QueryStamp::default())
                .unwrap();
            assert_eq!(batch.rows, page.rows);
            assert_eq!(batch.next.is_some(), offset + 1 < indices.len());
            after = batch.next;
        }
    }
}
