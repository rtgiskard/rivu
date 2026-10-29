use super::*;

fn page_query() -> Query {
    Query::OtherTracksPage {
        query: None,
        favorite: None,
        missing: None,
        sort: LibrarySort::Id,
        offset: 0,
        limit: MAX_QUERY_ROWS,
    }
}

fn page(core: &Core) -> Page<LibraryRow> {
    let ViewResponse::LibraryPage(page) = core.view(&page_query()).unwrap() else {
        panic!("Other must return library rows");
    };
    page
}

#[test]
fn current_roots_change_other_projection_and_stamps_but_not_persistent_tracks() {
    let (directory, mut core) = fixture();
    core.update_gui_config(GuiConfigChange::RemoveLibraryRoot(
        directory.path().to_path_buf(),
    ))
    .unwrap();
    let before = core
        .store
        .library_page(None, None, None, 0, MAX_QUERY_ROWS)
        .unwrap();
    let queue = core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap();
    let roots = core.state.configuration.config.library_roots.clone();
    let initial = core.state.query_revisions_for(&page_query());
    let initial_stamp = page_query().stamp(initial);
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(
        directory.path().to_path_buf(),
    ))
    .unwrap();
    assert_eq!(page(&core).total, 0);
    let scoped = core.state.query_revisions_for(&page_query());
    assert_ne!(initial_stamp, page_query().stamp(scoped));
    assert_eq!(initial.library, scoped.library);
    assert_ne!(initial.structure, scoped.structure);
    assert!(matches!(
        core.view(&Query::OtherTracksCount).unwrap(),
        ViewResponse::OtherTracksCount(0)
    ));
    assert!(matches!(
        core.view(&Query::OtherTracksLocate {
            track_id: 1,
            query: None,
            favorite: None,
            missing: None,
            sort: LibrarySort::Id,
        })
        .unwrap(),
        ViewResponse::LibraryLocate(LibraryLocateResult::OutsideRoot)
    ));
    core.update_gui_config(GuiConfigChange::RemoveLibraryRoot(
        directory.path().to_path_buf(),
    ))
    .unwrap();
    assert_eq!(core.state.configuration.config.library_roots, roots);
    assert_eq!(page(&core).total, 4);
    assert_eq!(
        core.store
            .library_page(None, None, None, 0, MAX_QUERY_ROWS)
            .unwrap(),
        before
    );
    assert_eq!(
        core.store.queue_page(None, 0, MAX_QUERY_ROWS).unwrap(),
        queue
    );
}

#[test]
fn other_collection_source_commands_resolve_execution_time_roots() {
    let (directory, mut core) = fixture();
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(
        directory.path().to_path_buf(),
    ))
    .unwrap();
    let before = core.store.queue_ids().unwrap();
    core.command(Command::EnqueueSources {
        other_tracks: true,
        directories: vec![],
        track_ids: vec![],
    })
    .unwrap();
    assert_eq!(core.store.queue_ids().unwrap(), before);
    core.update_gui_config(GuiConfigChange::RemoveLibraryRoot(
        directory.path().to_path_buf(),
    ))
    .unwrap();
    core.command(Command::EnqueueSources {
        other_tracks: true,
        directories: vec![],
        track_ids: vec![],
    })
    .unwrap();
    let rows = core
        .store
        .queue_page(None, before.len(), MAX_QUERY_ROWS)
        .unwrap()
        .rows;
    assert_eq!(
        rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    let tracks = core.state.library.track_total;
    core.command(Command::ClearQueue).unwrap();
    assert_eq!(core.state.library.track_total, tracks);
    assert_eq!(page(&core).total, 4);
}

#[test]
fn other_query_identity_excludes_page_demand_and_selects_root_revision() {
    let query = page_query();
    let mut demand = query.clone();
    if let Query::OtherTracksPage { offset, limit, .. } = &mut demand {
        *offset = 100;
        *limit = 1;
    }
    assert_eq!(query.identity(), demand.identity());
    let baseline = QueryRevisions {
        library: 3,
        structure: 5,
        queue: 7,
        ..Default::default()
    };
    for query in [
        query,
        Query::OtherTracksCount,
        Query::OtherTracksLocate {
            track_id: 1,
            query: None,
            favorite: None,
            missing: None,
            sort: LibrarySort::Id,
        },
    ] {
        let mut unrelated = baseline;
        unrelated.queue += 1;
        assert_eq!(query.stamp(baseline), query.stamp(unrelated));
        let mut roots = baseline;
        roots.structure += 1;
        assert_ne!(query.stamp(baseline), query.stamp(roots));
    }
}
