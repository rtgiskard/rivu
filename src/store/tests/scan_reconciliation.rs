use super::*;

#[test]
fn moved_id_preserves_override_and_delete_does_not_reuse() {
    let (directory, s) = db();
    let original = directory.path().join("a.mp3");
    let moved = directory.path().join("b.mp3");
    let replacement = directory.path().join("c.mp3");
    s.apply_scan(&scan(vec![rec(
        original.to_str().unwrap(),
        "same",
        "Raw",
        100.,
    )]))
    .unwrap();
    let id = all_tracks(&s)[0].id;
    s.edit_track(
        id,
        Some("Override"),
        Some("A"),
        Some("B"),
        Some(DecoderHint::Ffmpeg),
    )
    .unwrap();
    s.apply_scan(&scan(vec![rec(
        moved.to_str().unwrap(),
        "same",
        "Changed",
        100.,
    )]))
    .unwrap();
    let tracks = all_tracks(&s);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].path, moved);
    assert_eq!(tracks[0].id, id);
    assert_eq!(tracks[0].title, "Override");
    assert_eq!(tracks[0].decoder_hint, DecoderHint::Ffmpeg);
    s.remove_tracks(&[id]).unwrap();
    assert!(all_tracks(&s).is_empty());
    s.apply_scan(&scan(vec![rec(
        replacement.to_str().unwrap(),
        "new",
        "New",
        100.,
    )]))
    .unwrap();
    let tracks = all_tracks(&s);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].path, replacement);
    assert_ne!(tracks[0].id, id);
}

#[test]
fn shared_source_segments_keep_independent_ids_overrides_stats_and_missing_state() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let whole = rec("/music/shared.wav", "shared", "Whole", 180.0);
    let first = segment("/music/a.cue", 1, 0, Some(4500));
    let second = segment("/music/a.cue", 2, 4500, Some(9000));
    let other = segment("/music/b.cue", 1, 0, Some(4500));
    store
        .apply_scan(&ScanResult {
            segments: vec![first.clone(), second.clone(), other],
            ..scan(vec![whole])
        })
        .unwrap();
    let tracks = all_tracks(&store);
    let ids: Vec<_> = tracks.iter().map(|t| t.id).collect();
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 4);
    store
        .edit_track(ids[1], Some("Custom"), Some("Artist"), Some("Album"), None)
        .unwrap();
    store.mark_played(ids[1], 10).unwrap();
    store.increment_play_count(ids[1]).unwrap();
    let playlist = store.create_playlist("Segments").unwrap();
    store
        .add_playlist(playlist, &[ids[1], ids[2], ids[1], ids[3]])
        .unwrap();
    store
        .edit_track(ids[1], None, None, None, Some(DecoderHint::Native))
        .unwrap();
    store
        .edit_track(ids[2], None, None, None, Some(DecoderHint::Ffmpeg))
        .unwrap();
    let mut changed_source = rec("/music/replaced.wav", "shared", "Source", 180.0);
    changed_source.media.title = "Source title, not segment title".into();
    let changed_cue = CueSegment {
        sheet: "/music/a.cue".into(),
        number: 1,
        start_frame: 75,
        end_frame: Some(4500),
    };
    let changed = CueRecord::registered(
        std::sync::Arc::new(changed_source),
        changed_cue.clone(),
        RegisteredCueMetadata {
            title: "New raw title".into(),
            artist: String::new(),
            album: String::new(),
            track_number: None,
            disc_number: None,
            release_date: None,
        },
        Some(59.0),
    );
    let mut rescan = ScanResult {
        segments: vec![changed.clone(), second.clone()],
        ..scan(Vec::new())
    };
    rescan.roots = vec!["/music/a.cue".into()];
    store.apply_scan(&rescan).unwrap();
    let after = all_tracks(&store);
    assert_eq!(after.iter().map(|t| t.id).collect::<Vec<_>>(), ids);
    assert_eq!(after[1].title, "Custom");
    assert_eq!(after[1].cue, Some(changed_cue.clone()));
    assert_eq!(after[1].path, changed.source.path);
    assert_eq!(after[1].duration, Some(59.0));
    assert_eq!((after[1].play_count, after[1].last_played), (1, Some(10)));
    assert_eq!(after[0].decoder_hint, DecoderHint::Auto);
    assert_eq!(after[1].decoder_hint, DecoderHint::Native);
    assert_eq!(after[2].decoder_hint, DecoderHint::Ffmpeg);
    assert_eq!(after[3].decoder_hint, DecoderHint::Auto);
    for index in [0, 2, 3] {
        assert_eq!(
            (after[index].play_count, after[index].last_played),
            (0, None)
        );
        assert!(!after[index].missing);
    }
    let known = store.stored_scan_files(&[PathBuf::from("/")]).unwrap();
    let known = known.iter().find(|r| r.track_id == ids[1]).unwrap();
    assert_eq!(known.cue, Some(changed_cue));
    assert_eq!(known.media.as_ref().unwrap().title, "New raw title");
    rescan.segments = vec![second];
    store.apply_scan(&rescan).unwrap();
    let after = all_tracks(&store);
    assert!(after[1].missing);
    for index in [0, 2, 3] {
        assert!(!after[index].missing);
    }
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        [ids[1], ids[2], ids[1], ids[3]]
    );
    rescan.segments.push(changed);
    store.apply_scan(&rescan).unwrap();
    assert!(!all_tracks(&store)[1].missing);
    store.remove_tracks(&[ids[1]]).unwrap();
    assert_eq!(
        all_tracks(&store).iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![ids[0], ids[2], ids[3]]
    );
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|e| e.track_id)
            .collect::<Vec<_>>(),
        vec![ids[2], ids[3]]
    );
    assert!(store.history(10).unwrap().is_empty());
}

#[test]
fn whole_source_and_cue_segments_reconcile_independently() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let whole = rec("/music/shared.wav", "shared", "Whole", 180.0);
    let first = segment("/music/a.cue", 1, 0, Some(4500));
    store
        .apply_scan(&ScanResult {
            segments: vec![first.clone()],
            ..scan(vec![whole.clone()])
        })
        .unwrap();
    let mut result = ScanResult {
        segments: vec![first],
        ..scan(vec![whole.clone()])
    };
    store.apply_scan(&result).unwrap();
    assert!(all_tracks(&store).iter().all(|track| !track.missing));
    result.records = vec![std::sync::Arc::new(whole)];
    result.segments.clear();
    store.apply_scan(&result).unwrap();
    let tracks = all_tracks(&store);
    assert!(!tracks[0].missing);
    assert!(tracks[1].missing);
}

#[test]
fn depth_excluded_tracks_keep_missing_state_and_are_not_relocation_candidates() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let whole = rec("/music/deep/whole.wav", "whole", "Original", 10.0);
    let cue = segment("/music/deep/album.cue", 1, 0, None);
    let already_missing = rec(
        "/music/deep/already-missing.wav",
        "already-missing",
        "Already missing",
        10.0,
    );
    store
        .apply_scan(&ScanResult {
            segments: vec![cue.clone()],
            ..scan(vec![
                whole.clone(),
                already_missing.clone(),
                rec("/music/deeper/absent.wav", "prefix", "Other branch", 10.0),
            ])
        })
        .unwrap();
    let whole_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&whole.path))
        .unwrap()
        .unwrap();
    let cue_id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(cue.cue().sheet, 1))
        .unwrap()
        .unwrap();
    let missing_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&already_missing.path))
        .unwrap()
        .unwrap();
    store.set_favorite(&[whole_id, cue_id], true).unwrap();
    store
        .apply_scan(&ScanResult {
            roots: vec![already_missing.path.clone()],
            ..ScanResult::default()
        })
        .unwrap();
    let copy = rec("/music/copy.wav", "whole", "New copy", 10.0);
    let mut partial = scan(vec![copy.clone()]);
    partial
        .skipped_directories
        .push(crate::library::SkippedDirectory {
            root_index: 0,
            path: "/music/deep".into(),
        });
    store.apply_scan(&partial).unwrap();
    let preserved = store.track(whole_id).unwrap().unwrap();
    assert_eq!(preserved.path, whole.path);
    assert_eq!(preserved.title, "Original");
    assert!(!preserved.missing);
    assert!(preserved.favorite);
    let preserved_cue = store.track(cue_id).unwrap().unwrap();
    assert!(!preserved_cue.missing);
    assert!(preserved_cue.favorite);
    assert_eq!(
        preserved_cue.cue.as_ref().unwrap().sheet,
        Path::new("/music/deep/album.cue")
    );
    assert!(store.track(missing_id).unwrap().unwrap().missing);
    let copy_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&copy.path))
        .unwrap()
        .unwrap();
    assert_ne!(copy_id, whole_id);
    let outside_id = all_tracks(&store)
        .into_iter()
        .find(|track| track.path == Path::new("/music/deeper/absent.wav"))
        .unwrap()
        .id;
    assert!(store.track(outside_id).unwrap().unwrap().missing);
    store.apply_scan(&scan(Vec::new())).unwrap();
    assert!(store.track(whole_id).unwrap().unwrap().missing);
    assert!(store.track(cue_id).unwrap().unwrap().missing);
}

#[test]
fn explicit_nested_scans_override_only_ancestor_depth_exclusions() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let sibling = rec("/music/deep/sibling.wav", "sibling", "Sibling", 10.0);
    let nested = rec("/music/deep/nested/absent.wav", "nested", "Nested", 10.0);
    let beyond = rec(
        "/music/deep/nested/beyond/hidden.wav",
        "beyond",
        "Beyond",
        10.0,
    );
    store
        .apply_scan(&scan(vec![sibling.clone(), nested.clone(), beyond.clone()]))
        .unwrap();
    let mut partial = scan(Vec::new());
    partial.roots.push("/music/deep/nested".into());
    partial.skipped_directories = vec![
        crate::library::SkippedDirectory {
            root_index: 0,
            path: "/music/deep".into(),
        },
        crate::library::SkippedDirectory {
            root_index: 1,
            path: "/music/deep/nested/beyond".into(),
        },
    ];
    store.apply_scan(&partial).unwrap();
    for (record, missing) in [(sibling, false), (nested, true), (beyond, false)] {
        let id = all_tracks(&store)
            .into_iter()
            .find(|track| track.path == record.path)
            .unwrap()
            .id;
        assert_eq!(store.track(id).unwrap().unwrap().missing, missing);
    }
}

#[test]
fn outer_exclusions_do_not_hide_a_completed_nested_scan() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let deep = rec("/music/nested/covered/deep.wav", "deep", "Deep", 10.0);
    store.apply_scan(&scan(vec![deep.clone()])).unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&deep.path))
        .unwrap()
        .unwrap();
    let mut partial = scan(Vec::new());
    partial.roots.push("/music/nested".into());
    partial
        .skipped_directories
        .push(crate::library::SkippedDirectory {
            root_index: 0,
            path: "/music/nested/covered".into(),
        });
    store.apply_scan(&partial).unwrap();
    assert!(store.track(id).unwrap().unwrap().missing);
}

#[test]
fn incomplete_scan_preserves_absent_identities_and_avoids_relocation() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let old = rec("/music/old.wav", "same", "Original", 10.0);
    store.apply_scan(&scan(vec![old.clone()])).unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&old.path))
        .unwrap()
        .unwrap();
    store.set_favorite(&[id], true).unwrap();
    let copy = rec("/music/copy.wav", "same", "Copy", 10.0);
    let mut incomplete = scan(vec![copy.clone()]);
    incomplete.scan_incomplete = true;
    store.apply_scan(&incomplete).unwrap();
    let preserved = store.track(id).unwrap().unwrap();
    assert_eq!(preserved.path, old.path);
    assert!(preserved.favorite);
    assert!(!preserved.missing);
    assert_ne!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&copy.path))
            .unwrap()
            .unwrap(),
        id
    );
    store.apply_scan(&scan(vec![copy])).unwrap();
    assert!(store.track(id).unwrap().unwrap().missing);
}

#[test]
fn invalid_fingerprints_roll_back_complete_and_incomplete_scans() {
    for incomplete in [false, true] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut original = rec("/music/old.wav", "old", "Original", 10.0);
        store.apply_scan(&scan(vec![original.clone()])).unwrap();
        let id = store
            .track_id_for_identity(crate::store::TrackIdentity::File(&original.path))
            .unwrap()
            .unwrap();
        store.set_favorite(&[id], true).unwrap();
        let before = store.track(id).unwrap().unwrap();
        original.media.title = "Uncommitted".into();
        let mut invalid = rec("/music/new.wav", "new", "New", 10.0);
        invalid.fingerprint = Some("not-a-fingerprint".into());
        let mut result = scan(vec![original, invalid]);
        result.scan_incomplete = incomplete;
        assert!(store.apply_scan(&result).is_err());
        assert_eq!(store.track_count().unwrap(), 1);
        assert_eq!(store.track(id).unwrap().unwrap(), before);
        store.track_cache.borrow_mut().entries.clear();
        assert_eq!(store.track(id).unwrap().unwrap(), before);
    }
}

#[test]
fn uncertain_scopes_preserve_owned_rows_and_reject_relocation_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let old_path = root.join("failed/whole.wav");
    let old_sheet = root.join("failed/old.cue");
    let copy_path = root.join("copy.wav");
    let new_sheet = root.join("new.cue");
    for (uncertainty, protect_file, protect_cue) in [
        (ScanUncertainty::Root { root_index: 0 }, true, true),
        (
            ScanUncertainty::Subtree {
                root_index: 0,
                path: root.join("failed"),
            },
            true,
            true,
        ),
        (ScanUncertainty::File(old_path.clone()), true, false),
        (ScanUncertainty::CueSheet(old_sheet.clone()), false, true),
    ] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let old_file = rec(old_path.to_str().unwrap(), "whole", "Original", 10.0);
        let old_cue = segment(old_sheet.to_str().unwrap(), 1, 0, Some(4500));
        let mut baseline = ScanResult {
            segments: vec![old_cue.clone()],
            ..scan(vec![old_file.clone()])
        };
        baseline.roots = vec![root.clone()];
        store.apply_scan(&baseline).unwrap();
        let file_id = store
            .track_id_for_identity(crate::store::TrackIdentity::File(&old_path))
            .unwrap()
            .unwrap();
        let cue_id = store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(&old_sheet, 1))
            .unwrap()
            .unwrap();
        store.set_favorite(&[file_id, cue_id], true).unwrap();
        let copy = rec(copy_path.to_str().unwrap(), "whole", "Copy", 10.0);
        let new_cue = segment(new_sheet.to_str().unwrap(), 1, 0, Some(4500));
        let mut partial = ScanResult {
            segments: vec![new_cue],
            ..scan(vec![copy])
        };
        partial.roots = vec![root.clone()];
        partial.uncertainties.push(uncertainty);
        store.apply_scan(&partial).unwrap();
        if protect_file {
            let preserved = store.track(file_id).unwrap().unwrap();
            assert_eq!(preserved.path, old_file.path);
            assert!(!preserved.missing);
            assert!(preserved.favorite);
            assert_ne!(
                store
                    .track_id_for_identity(crate::store::TrackIdentity::File(&copy_path))
                    .unwrap()
                    .unwrap(),
                file_id
            );
        }
        if protect_cue {
            let preserved = store.track(cue_id).unwrap().unwrap();
            assert_eq!(
                preserved.cue,
                Some(CueSegment {
                    sheet: old_cue.cue().sheet.clone(),
                    number: 1,
                    start_frame: 0,
                    end_frame: Some(4500),
                })
            );
            assert!(!preserved.missing);
            assert!(preserved.favorite);
            assert_ne!(
                store
                    .track_id_for_identity(crate::store::TrackIdentity::Cue(&new_sheet, 1))
                    .unwrap()
                    .unwrap(),
                cue_id
            );
        }
        store.remove_missing_tracks().unwrap();
        assert!(store.track(file_id).unwrap().is_some());
        assert!(store.track(cue_id).unwrap().is_some());
    }
}

#[test]
fn nearest_explicit_root_owns_uncertain_directory_coverage() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let sibling = rec("/music/deep/sibling.wav", "sibling", "Sibling", 10.0);
    let nested = rec("/music/deep/nested/absent.wav", "nested", "Nested", 10.0);
    let beyond = segment("/music/deep/nested/beyond/hidden.cue", 1, 0, Some(4500));
    store
        .apply_scan(&ScanResult {
            segments: vec![beyond.clone()],
            ..scan(vec![sibling.clone(), nested.clone()])
        })
        .unwrap();
    let sibling_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&sibling.path))
        .unwrap()
        .unwrap();
    let nested_id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&nested.path))
        .unwrap()
        .unwrap();
    let beyond_id = store
        .track_id_for_identity(crate::store::TrackIdentity::Cue(beyond.cue().sheet, 1))
        .unwrap()
        .unwrap();
    let mut partial = scan(Vec::new());
    partial.roots.push("/music/deep/nested".into());
    partial.uncertainties = vec![
        ScanUncertainty::Root { root_index: 0 },
        ScanUncertainty::Subtree {
            root_index: 0,
            path: "/music/deep/nested".into(),
        },
        ScanUncertainty::Subtree {
            root_index: 1,
            path: "/music/deep/nested/beyond".into(),
        },
    ];
    store.apply_scan(&partial).unwrap();
    assert!(!store.track(sibling_id).unwrap().unwrap().missing);
    assert!(store.track(nested_id).unwrap().unwrap().missing);
    assert!(!store.track(beyond_id).unwrap().unwrap().missing);
}

#[test]
fn incomplete_scan_does_not_relocate_unknown_sources_outside_requested_roots() {
    let directory = tempfile::tempdir().unwrap();
    let old_path = directory.path().join("outside/old.wav");
    let requested = directory.path().join("requested");
    let copy_path = requested.join("copy.wav");
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec(
            old_path.to_str().unwrap(),
            "same",
            "Original",
            10.0,
        )]))
        .unwrap();
    let id = store
        .track_id_for_identity(crate::store::TrackIdentity::File(&old_path))
        .unwrap()
        .unwrap();
    let mut partial = scan(vec![rec(copy_path.to_str().unwrap(), "same", "Copy", 10.0)]);
    partial.roots = vec![requested];
    partial.scan_incomplete = true;
    store.apply_scan(&partial).unwrap();
    assert_eq!(store.track(id).unwrap().unwrap().path, old_path);
    assert_ne!(
        store
            .track_id_for_identity(crate::store::TrackIdentity::File(&copy_path))
            .unwrap()
            .unwrap(),
        id
    );
}

#[test]
fn fingerprints_never_merge_full_files_or_different_cue_segments() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cases = [
        (None, Some((1, 0, Some(4500))), false),
        (Some((1, 0, Some(4500))), None, false),
        (Some((1, 0, Some(4500))), Some((2, 0, Some(4500))), false),
        (Some((1, 0, Some(4500))), Some((1, 75, Some(4500))), false),
        (Some((1, 0, Some(4500))), Some((1, 0, None)), false),
        (Some((1, 0, Some(4500))), Some((1, 0, Some(4500))), true),
    ];
    for (index, (before, after, same_id)) in cases.into_iter().enumerate() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let make_scan = |value: Option<(u32, u64, Option<u64>)>, name: &str, title: &str| {
            let mut source = rec("", "same-hash", title, 60.0);
            source.path = root.join(format!("{index}-{name}.wav"));
            let mut result = scan(Vec::new());
            result.roots = vec![root.to_path_buf()];
            if let Some((number, start_frame, end_frame)) = value {
                result.segments.push(CueRecord::registered(
                    std::sync::Arc::new(source),
                    CueSegment {
                        sheet: root.join(format!("{index}-{name}.cue")),
                        number,
                        start_frame,
                        end_frame,
                    },
                    RegisteredCueMetadata {
                        title: title.into(),
                        artist: String::new(),
                        album: String::new(),
                        track_number: None,
                        disc_number: None,
                        release_date: None,
                    },
                    Some(60.0),
                ));
            } else {
                result.records.push(std::sync::Arc::new(source));
            }
            result
        };
        let mut result = make_scan(before, "old", "Old");
        store.apply_scan(&result).unwrap();
        let old_id = all_tracks(&store)[0].id;
        result = make_scan(after, "new", "New");
        store.apply_scan(&result).unwrap();
        let tracks = all_tracks(&store);
        let new_track = tracks.iter().find(|t| t.title == "New").unwrap();
        assert_eq!(new_track.id == old_id, same_id, "case {index}");
        assert_eq!(tracks.len(), if same_id { 1 } else { 2 });
        assert!(!new_track.missing);
        if !same_id {
            assert!(tracks.iter().find(|t| t.id == old_id).unwrap().missing);
        }
    }
}

#[test]
fn cue_relocation_requires_absent_sheet_not_absent_shared_source() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let sheet = root.join("old.cue");
    let source = root.join("shared.wav");
    fs::write(&sheet, b"sheet").unwrap();
    fs::write(&source, b"source").unwrap();
    let mut old = segment(sheet.to_str().unwrap(), 1, 0, Some(4500));
    std::sync::Arc::make_mut(&mut old.source).path = source.clone();
    let mut moved = segment(root.join("new.cue").to_str().unwrap(), 1, 0, Some(4500));
    moved.source = old.source.clone();
    for sheet_exists in [true, false] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut result = ScanResult {
            segments: vec![old.clone()],
            ..scan(Vec::new())
        };
        result.roots = vec![root.to_path_buf()];
        store.apply_scan(&result).unwrap();
        let id = all_tracks(&store)[0].id;
        store
            .edit_track(id, Some("Override"), Some("Artist"), Some("Album"), None)
            .unwrap();
        if !sheet_exists {
            fs::remove_file(&sheet).unwrap();
        }
        result.segments = vec![moved.clone()];
        store.apply_scan(&result).unwrap();
        let tracks = all_tracks(&store);
        let expected_cue = Some(CueSegment {
            sheet: moved.cue().sheet.clone(),
            number: 1,
            start_frame: 0,
            end_frame: Some(4500),
        });
        let new = tracks.iter().find(|t| t.cue == expected_cue).unwrap();
        assert_eq!(new.id == id, !sheet_exists);
        if !sheet_exists {
            assert_eq!(new.title, "Override");
        }
    }
}

#[test]
fn ambiguous_cue_relocation_preserves_ids_and_user_data_on_both_sides() {
    let directory = tempfile::tempdir().unwrap();
    for (old_count, new_count) in [(2, 1), (1, 2), (2, 2)] {
        for reverse in [false, true] {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let make = |prefix: &str, index| {
                segment(
                    directory
                        .path()
                        .join(format!("{prefix}-{index}.cue"))
                        .to_str()
                        .unwrap(),
                    1,
                    0,
                    Some(4500),
                )
            };
            let mut result = ScanResult {
                segments: (0..old_count).map(|index| make("old", index)).collect(),
                ..scan(Vec::new())
            };
            result.roots = vec![directory.path().to_path_buf()];
            store.apply_scan(&result).unwrap();
            let old_ids: Vec<_> = all_tracks(&store).iter().map(|track| track.id).collect();
            store.set_favorite(&old_ids, true).unwrap();
            for id in &old_ids {
                store
                    .edit_track(*id, Some("Owned title"), None, None, None)
                    .unwrap();
            }
            let playlist = store.create_playlist("Owned references").unwrap();
            store.add_playlist(playlist, &old_ids).unwrap();
            result.segments = (0..new_count).map(|index| make("new", index)).collect();
            if reverse {
                result.segments.reverse();
            }
            store.apply_scan(&result).unwrap();
            let tracks = all_tracks(&store);
            assert_eq!(tracks.len(), old_count + new_count);
            for id in &old_ids {
                let old = store.track(*id).unwrap().unwrap();
                assert!(old.missing && old.favorite);
                assert_eq!(old.title, "Owned title");
            }
            for new in tracks.iter().filter(|track| !old_ids.contains(&track.id)) {
                assert!(!new.missing && !new.favorite);
                assert_eq!(new.title, "Track 1");
            }
            assert_eq!(
                playlist_entries(&store, playlist)
                    .iter()
                    .map(|entry| entry.track_id)
                    .collect::<Vec<_>>(),
                old_ids
            );
        }
    }
}

#[test]
fn exact_cue_identities_do_not_count_as_new_relocation_destinations() {
    let directory = tempfile::tempdir().unwrap();
    for reverse in [false, true] {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let make = |name: &str| {
            segment(
                directory.path().join(name).to_str().unwrap(),
                1,
                0,
                Some(4500),
            )
        };
        let old = make("old.cue");
        let exact = make("existing.cue");
        let mut result = ScanResult {
            segments: vec![old.clone(), exact.clone()],
            ..scan(Vec::new())
        };
        result.roots = vec![directory.path().to_path_buf()];
        store.apply_scan(&result).unwrap();
        let old_id = store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(old.cue().sheet, 1))
            .unwrap()
            .unwrap();
        let exact_id = store
            .track_id_for_identity(crate::store::TrackIdentity::Cue(exact.cue().sheet, 1))
            .unwrap()
            .unwrap();
        store
            .edit_track(old_id, Some("Migrated title"), None, None, None)
            .unwrap();
        store
            .edit_track(exact_id, Some("Exact title"), None, None, None)
            .unwrap();
        let destination = make("new.cue");
        result.segments = vec![destination.clone(), exact.clone()];
        if reverse {
            result.segments.reverse();
        }
        store.apply_scan(&result).unwrap();
        assert_eq!(all_tracks(&store).len(), 2);
        let migrated = store.track(old_id).unwrap().unwrap();
        assert_eq!(
            migrated.cue,
            Some(CueSegment {
                sheet: destination.cue().sheet.clone(),
                number: 1,
                start_frame: 0,
                end_frame: Some(4500),
            })
        );
        assert_eq!(migrated.title, "Migrated title");
        assert!(!migrated.missing);
        let preserved = store.track(exact_id).unwrap().unwrap();
        assert_eq!(
            preserved.cue,
            Some(CueSegment {
                sheet: exact.cue().sheet.clone(),
                number: 1,
                start_frame: 0,
                end_frame: Some(4500),
            })
        );
        assert_eq!(preserved.title, "Exact title");
        assert!(!preserved.missing);
    }
}

#[test]
fn cue_relocation_keeps_unique_segments_when_another_segment_is_ambiguous() {
    let directory = tempfile::tempdir().unwrap();
    let old_sheet = directory.path().join("old.cue");
    let duplicate_sheet = directory.path().join("duplicate.cue");
    let new_sheet = directory.path().join("new.cue");
    let store = Store::open(Path::new(":memory:")).unwrap();
    let make = |sheet: &Path, number| {
        segment(
            sheet.to_str().unwrap(),
            number,
            u64::from(number - 1) * 4500,
            (number < 3).then_some(u64::from(number) * 4500),
        )
    };
    let mut original = ScanResult {
        segments: (1..=3).map(|number| make(&old_sheet, number)).collect(),
        ..scan(Vec::new())
    };
    original.roots = vec![directory.path().to_path_buf()];
    original.segments.push(make(&duplicate_sheet, 2));
    store.apply_scan(&original).unwrap();
    let ids: Vec<_> = (1..=3)
        .map(|number| {
            all_tracks(&store)
                .iter()
                .find(|track| {
                    track
                        .cue
                        .as_ref()
                        .is_some_and(|cue| cue.sheet == old_sheet && cue.number == number)
                })
                .unwrap()
                .id
        })
        .collect();
    store.set_favorite(&ids, true).unwrap();
    let playlist = store.create_playlist("Saved segments").unwrap();
    store.add_playlist(playlist, &ids).unwrap();
    let mut moved = ScanResult {
        segments: (1..=3)
            .rev()
            .map(|number| make(&new_sheet, number))
            .collect(),
        ..scan(Vec::new())
    };
    moved.roots = original.roots;
    store.apply_scan(&moved).unwrap();
    let tracks = all_tracks(&store);
    for number in 1..=3 {
        let track = tracks
            .iter()
            .find(|track| {
                track
                    .cue
                    .as_ref()
                    .is_some_and(|cue| cue.sheet == new_sheet && cue.number == number)
            })
            .unwrap();
        let old_id = ids[(number - 1) as usize];
        if number == 2 {
            assert_ne!(track.id, old_id);
            assert!(!track.favorite);
            let old = store.track(old_id).unwrap().unwrap();
            assert!(old.missing && old.favorite);
        } else {
            assert_eq!(track.id, old_id);
            assert!(track.favorite && !track.missing);
        }
    }
    assert_eq!(
        playlist_entries(&store, playlist)
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        ids
    );
}

#[test]
fn invalid_cue_database_range_leaves_entire_scan_unchanged() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .apply_scan(&scan(vec![rec(
            "/music/original.wav",
            "old",
            "Original",
            60.0,
        )]))
        .unwrap();
    let invalid = segment("/music/a.cue", 1, u64::MAX, None);
    assert!(
        store
            .apply_scan(&ScanResult {
                segments: vec![invalid],
                ..scan(vec![rec("/music/new.wav", "new", "New", 60.0)])
            })
            .is_err()
    );
    let tracks = all_tracks(&store);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Original");
    assert!(!tracks[0].missing);
}

#[test]
fn audio_spec_rescans_refresh_queued_projection_without_changing_occurrences() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let mut record = rec("/music/specs.flac", "specs", "Specs", 90.0);
    store.apply_scan(&scan(vec![record.clone()])).unwrap();
    let id = all_tracks(&store)[0].id;
    store.enqueue_tracks(&[id, id]).unwrap();
    let before = store.queue_page(None, 0, 2).unwrap();
    for field in 0..3 {
        match field {
            0 => record.media.channels = 6,
            1 => record.media.sample_rate = 96_000,
            _ => record.media.bits_per_sample = Some(24),
        }
        let impacts = store.apply_scan(&scan(vec![record.clone()])).unwrap();
        assert!(impacts.library && impacts.queue);
        assert!(!impacts.structure);
        let after = store.queue_page(None, 0, 2).unwrap();
        assert_eq!(before.total, 2);
        assert_eq!(before.rows.len(), 2);
        assert_eq!(after.total, before.total);
        assert_eq!(after.rows.len(), before.rows.len());
        for (old, new) in before.rows.iter().zip(&after.rows) {
            assert_eq!(
                (old.id, old.track_id, old.position),
                (new.id, new.track_id, new.position)
            );
            assert_eq!(new.track.channels, record.media.channels);
            assert_eq!(new.track.sample_rate, record.media.sample_rate);
            assert_eq!(new.track.bits_per_sample, record.media.bits_per_sample);
        }
        assert!(!store.apply_scan(&scan(vec![record.clone()])).unwrap().queue);
    }
}
