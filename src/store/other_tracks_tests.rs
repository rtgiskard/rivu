use super::*;
use rivu_protocol::model::{LibraryLocateResult, LibrarySort};
use std::path::{Path, PathBuf};

fn fixture() -> Store {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store.conn.execute_batch(
        "INSERT INTO tracks(id,path,raw_title,raw_artist,raw_album,favorite,missing,play_count,cue_sheet,cue_number,cue_start_frame,cue_end_frame) VALUES
         (1,'/music/a.wav','Needle A','Artist','Z',1,0,2,NULL,NULL,NULL,NULL),
         (2,'/music/deep/image.wav','Needle B','Artist','A',0,1,5,'/music/deep/image.cue',1,0,4500),
         (3,'/musical/out.wav','Needle C','Artist','B',1,1,8,NULL,NULL,NULL,NULL),
         (4,'/音楽/曲.wav','曲','Artist','C',0,0,4,NULL,NULL,NULL,NULL),
         (5,'/m%_usic/literal.wav','Needle D','Artist','D',1,0,1,NULL,NULL,NULL,NULL),
         (6,'/other/image.wav','Needle E','Artist','A',1,0,9,'/other/image.cue',1,0,4500),
         (7,'/other/image.wav','Needle F','Artist','A',1,0,7,'/other/image.cue',2,4500,9000);"
    ).unwrap();
    store
}

fn ids(store: &Store, roots: &[PathBuf], sort: LibrarySort) -> Vec<i64> {
    store
        .other_tracks_page(roots, None, None, None, sort, 0, 100)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| row.id)
        .collect()
}

#[test]
fn other_projection_is_root_complement_without_membership_or_reference_mutation() {
    let store = fixture();
    store.enqueue_tracks(&[3, 1, 3]).unwrap();
    let playlist = store.create_playlist_with_tracks("Keep", &[1, 6]).unwrap();
    let queue = store.queue_page(None, 0, 100).unwrap();
    let playlist_rows = store.playlist_entries_page(playlist, None, 0, 100).unwrap();
    let before = store.library_page(None, None, None, 0, 100).unwrap();
    assert_eq!(ids(&store, &[], LibrarySort::Id), [1, 2, 3, 4, 5, 6, 7]);
    let roots = ["/music/./", "/music/deep/../", "/音楽"].map(PathBuf::from);
    assert_eq!(ids(&store, &roots, LibrarySort::Id), [3, 5, 6, 7]);
    assert_eq!(store.other_tracks_count(&roots).unwrap(), 4);
    assert_eq!(
        store.other_tracks_sources(&roots).unwrap(),
        [
            PathBuf::from("/m%_usic/literal.wav"),
            PathBuf::from("/musical/out.wav"),
            PathBuf::from("/other/image.wav"),
        ]
    );
    assert!(ids(&store, &["/".into()], LibrarySort::Id).is_empty());
    assert_eq!(
        ids(&store, &["/m%_usic".into()], LibrarySort::Id),
        [1, 2, 3, 4, 6, 7]
    );
    assert_eq!(ids(&store, &[], LibrarySort::Id), [1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(
        store.library_page(None, None, None, 0, 100).unwrap(),
        before
    );
    assert_eq!(store.queue_page(None, 0, 100).unwrap(), queue);
    assert_eq!(
        store.playlist_entries_page(playlist, None, 0, 100).unwrap(),
        playlist_rows
    );
}

#[test]
fn other_filters_sort_exact_pages_locations_and_source_paths_agree() {
    let store = fixture();
    let roots = [PathBuf::from("/music")];
    for sort in [LibrarySort::Id, LibrarySort::Album, LibrarySort::MostPlayed] {
        for favorite in [None, Some(false), Some(true)] {
            for missing in [None, Some(false), Some(true)] {
                let page = store
                    .other_tracks_page(&roots, Some("needle"), favorite, missing, sort, 0, 100)
                    .unwrap();
                assert_eq!(page.total, page.rows.len());
                for offset in 0..=page.total + 1 {
                    let slice = store
                        .other_tracks_page(
                            &roots,
                            Some("needle"),
                            favorite,
                            missing,
                            sort,
                            offset,
                            1,
                        )
                        .unwrap();
                    assert_eq!(slice.total, page.total);
                    assert_eq!(
                        slice.rows,
                        page.rows.get(offset..offset + 1).unwrap_or_default()
                    );
                }
                assert!(
                    store
                        .other_tracks_page(&roots, Some("needle"), favorite, missing, sort, 0, 0)
                        .unwrap()
                        .rows
                        .is_empty()
                );
                for (offset, row) in page.rows.iter().enumerate() {
                    assert!(!row.path.as_os_str().is_empty());
                    let LibraryLocateResult::Located(location) = store
                        .other_tracks_location(
                            &roots,
                            row.id,
                            Some("needle"),
                            favorite,
                            missing,
                            sort,
                        )
                        .unwrap()
                    else {
                        panic!("matching row must locate");
                    };
                    assert_eq!(
                        (location.offset, location.total, location.child),
                        (offset, page.total, None)
                    );
                }
            }
        }
    }
    assert_eq!(
        ids(&store, &roots, LibrarySort::MostPlayed),
        [6, 3, 7, 4, 5]
    );
    assert_eq!(
        store
            .other_tracks_location(&roots, 1, None, None, None, LibrarySort::Id)
            .unwrap(),
        LibraryLocateResult::OutsideRoot
    );
    assert_eq!(
        store
            .other_tracks_location(&roots, 3, None, None, Some(false), LibrarySort::Id)
            .unwrap(),
        LibraryLocateResult::Filtered
    );
    assert_eq!(
        store
            .other_tracks_location(&roots, 999, None, None, None, LibrarySort::Id)
            .unwrap(),
        LibraryLocateResult::Unavailable
    );
    assert_eq!(
        store
            .other_tracks_page(
                &[],
                None,
                None,
                None,
                LibrarySort::Id,
                usize::MAX,
                usize::MAX
            )
            .unwrap()
            .total,
        7
    );
}

#[test]
fn other_bulk_sources_union_with_directories_and_ids_without_duplicate_members() {
    let store = fixture();
    let roots = [PathBuf::from("/music")];
    store
        .enqueue_sources(&["/music/deep".into()], &[3, 3], Some(&roots))
        .unwrap();
    assert_eq!(
        store
            .queue_page(None, 0, 100)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.track_id)
            .collect::<Vec<_>>(),
        [2, 3, 4, 5, 6, 7]
    );
    let playlist = store.create_playlist("All").unwrap();
    assert!(
        store
            .add_playlist_sources(playlist, &["/music".into()], &[], Some(&roots))
            .unwrap()
    );
    assert_eq!(
        store.playlist_track_ids(playlist, 100).unwrap(),
        [1, 2, 3, 4, 5, 6, 7]
    );
    let before = store.queue_state().unwrap();
    assert!(
        store
            .insert_queue_sources(&[], &[], Some(&[]), 0, before.revision.wrapping_add(1))
            .is_err()
    );
    assert_eq!(store.queue_state().unwrap(), before);
    store
        .insert_queue_sources(&[], &[], Some(&["/".into()]), before.total, before.revision)
        .unwrap();
    assert_eq!(store.queue_state().unwrap(), before);
    let other = store.create_playlist("Subset").unwrap();
    assert!(
        store
            .insert_playlist_sources(other, &[], &[], Some(&roots), 0)
            .unwrap()
    );
    assert_eq!(
        store.playlist_track_ids(other, 100).unwrap(),
        [3, 4, 5, 6, 7]
    );
}

#[cfg(unix)]
#[test]
fn other_scope_keeps_logical_symlink_spellings_distinct() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let alias = directory.path().join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO tracks(id,path) VALUES(1,?1),(2,?2)",
            rusqlite::params![
                real.join("track.wav").to_str().unwrap(),
                alias.join("track.wav").to_str().unwrap()
            ],
        )
        .unwrap();
    assert_eq!(ids(&store, &[real], LibrarySort::Id), [2]);
    assert_eq!(ids(&store, &[alias], LibrarySort::Id), [1]);
}
