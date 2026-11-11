use super::tests::{playlist_view, query_snapshot, queue_row, response, track};
use super::*;
use rivu_protocol::model::{
    LibraryLocateResult, LibraryLocation, LibrarySort, Page, PlaylistEntryLocation,
};
use std::path::Path;

fn key(ui: &mut UiState, state: &TuiSnapshot, code: KeyCode) -> KeyAction {
    ui.key_action(KeyEvent::from(code), state)
}

fn submit(ui: &mut UiState, state: &TuiSnapshot, query: &str) {
    ui.open_search();
    for character in query.chars() {
        assert!(matches!(
            key(ui, state, KeyCode::Char(character)),
            KeyAction::Ignored
        ));
    }
    key(ui, state, KeyCode::Enter);
    assert!(!ui.search.editing);
}

fn result(request: &QueryRequest, view: ViewResponse) -> QueryResult {
    QueryResult {
        revisions: request.demand.stamp.revisions(),
        view,
    }
}

fn library_fixture() -> (UiState, TuiSnapshot) {
    let mut state = query_snapshot();
    state.system.query_batch_size = 2;
    let mut ui = UiState {
        view: View::Tree(LibraryTree::new(&state.system.library_roots)),
        ..UiState::default()
    };
    submit(&mut ui, &state, "active");
    let request = ui.query_request(&state).unwrap();
    assert!(ui.apply_query(&state, &request, Ok(response(&request, 12))));
    (ui, state)
}

#[test]
fn draft_never_submits_and_enter_only_applies_without_playing() {
    let (mut ui, state) = library_fixture();
    let pending = ui.query_request(&state).unwrap();
    ui.open_search();
    for _ in 0.."active".len() {
        key(&mut ui, &state, KeyCode::Backspace);
    }
    for character in " next ".chars() {
        key(&mut ui, &state, KeyCode::Char(character));
    }
    assert_eq!(ui.search.applied_query, "active");
    assert_eq!(ui.query_request(&state), Some(pending.clone()));
    assert!(ui.apply_query(&state, &pending, Ok(response(&pending, 12))));
    assert!(matches!(
        key(&mut ui, &state, KeyCode::Enter),
        KeyAction::Ignored
    ));
    let fresh = ui.query_request(&state).unwrap();
    assert!(
        matches!(fresh.demand.query(), Query::LibraryLocate { track_id: 0, query: Some(query), .. } if query == "next")
    );
    let View::LibraryResults(rows) = &ui.view else {
        panic!("library results");
    };
    assert!(rows.pages.is_empty());
    assert!(ui.apply_query(&state, &fresh, Ok(response(&fresh, 12))));
    let page = ui.query_request(&state).unwrap();
    assert!(ui.apply_query(&state, &page, Ok(response(&page, 12))));
    assert!(matches!(
        key(&mut ui, &state, KeyCode::Enter),
        KeyAction::Command(Command::Play { track_id: 0 })
    ));
}

#[test]
fn every_printable_shortcut_is_editor_text_and_backspace_is_not_parent() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: View::Tree(LibraryTree {
            directory: "/music/album".into(),
            ..LibraryTree::default()
        }),
        ..UiState::default()
    };
    ui.open_search();
    for character in " m q?e/春".chars() {
        assert!(matches!(
            key(&mut ui, &state, KeyCode::Char(character)),
            KeyAction::Ignored
        ));
    }
    assert_eq!(ui.search.query, " m q?e/春");
    key(&mut ui, &state, KeyCode::Backspace);
    assert_eq!(ui.search.query, " m q?e/");
    assert!(matches!(&ui.view, View::Tree(tree) if tree.directory == Path::new("/music/album")));
    key(&mut ui, &state, KeyCode::F(1));
    assert!(ui.show_help);
    key(&mut ui, &state, KeyCode::Esc);
    assert!(ui.search.editing);
    assert!(matches!(
        ui.key_action(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &state
        ),
        KeyAction::Quit
    ));
}

#[test]
fn applying_or_clearing_rejects_old_responses_even_when_query_text_returns() {
    let (mut ui, state) = library_fixture();
    let old = ui.query_request(&state).unwrap();
    ui.open_search();
    ui.search.query = "replacement".into();
    key(&mut ui, &state, KeyCode::Enter);
    assert!(!ui.apply_query(&state, &old, Ok(response(&old, 12))));
    ui.open_search();
    ui.search.query = "active".into();
    key(&mut ui, &state, KeyCode::Enter);
    let first = ui.query_request(&state).unwrap();
    assert!(ui.apply_query(&state, &first, Ok(response(&first, 12))));
    let page = ui.query_request(&state).unwrap();
    assert!(ui.apply_query(&state, &page, Ok(response(&page, 12))));
    let fresh = ui.query_request(&state).unwrap();
    assert_eq!(fresh.demand.query(), old.demand.query());
    assert!(!ui.apply_query(&state, &old, Ok(response(&old, 12))));
    key(&mut ui, &state, KeyCode::Esc);
    assert!(!ui.search.active);
    assert!(matches!(ui.view, View::Tree(_)));
    assert!(!ui.apply_query(&state, &fresh, Ok(response(&fresh, 12))));
}

#[test]
fn empty_draft_waits_for_enter_and_global_track_cannot_select_a_tree_container() {
    let (mut ui, state) = library_fixture();
    ui.open_search();
    ui.search.query = "  ".into();
    assert!(
        matches!(ui.query_request(&state).unwrap().demand.query(), Query::LibraryPage { query: Some(query), .. } if query == "active")
    );
    key(&mut ui, &state, KeyCode::Enter);
    assert!(!ui.search.active);
    assert!(
        matches!(&ui.view, View::Tree(tree) if tree.directory.as_os_str().is_empty() && tree.entries.selection.selected() == Some(0))
    );
}

#[test]
fn filtered_queue_actions_use_occurrences_and_refresh_uses_filtered_offsets() {
    let mut state = query_snapshot();
    state.system.query_batch_size = 1;
    state.queue.total = 20;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    submit(&mut tui.ui, &state, "duplicate");
    let first = tui.ui.query_request(&state).unwrap();
    assert!(
        matches!(first.demand.query(), Query::QueuePage { query: Some(query), offset: 0, .. } if query == "duplicate")
    );
    let mut row = queue_row(8, 101);
    row.offset = 0;
    assert!(tui.ui.apply_query(
        &state,
        &first,
        Ok(result(
            &first,
            ViewResponse::QueuePage(Page {
                total: 2,
                rows: vec![row]
            })
        ))
    ));
    key(&mut tui.ui, &state, KeyCode::Down);
    let second = tui.ui.query_request(&state).unwrap();
    let mut duplicate = queue_row(17, 202);
    duplicate.offset = 1;
    assert!(tui.ui.apply_query(
        &state,
        &second,
        Ok(result(
            &second,
            ViewResponse::QueuePage(Page {
                total: 2,
                rows: vec![duplicate.clone()]
            })
        ))
    ));
    assert!(matches!(
        key(&mut tui.ui, &state, KeyCode::Enter),
        KeyAction::Command(Command::PlayQueue { queue_id: 202 })
    ));
    assert!(matches!(
        key(&mut tui.ui, &state, KeyCode::Delete),
        KeyAction::Command(Command::RemoveQueue { queue_id: 202 })
    ));
    let mut next = state.clone();
    next.queue.revision += 1;
    next.queue.total = 19;
    tui.apply_snapshot(next.clone());
    assert_eq!(tui.ui.queue.total, 2);
    let lookup = tui.ui.query_request(&next).unwrap();
    assert!(
        matches!(lookup.demand.query(), Query::QueueEntries { query: Some(query), queue_ids } if query == "duplicate" && queue_ids == vec![202])
    );
    duplicate.position = 16;
    duplicate.offset = 0;
    assert!(tui.ui.apply_query(
        &next,
        &lookup,
        Ok(result(&lookup, ViewResponse::QueueEntries(vec![duplicate])))
    ));
    assert_eq!(tui.ui.queue.selection.selected(), Some(0));
    let page = tui.ui.query_request(&next).unwrap();
    assert_eq!(page.demand.offset, 0);
    assert!(tui.ui.apply_query(
        &next,
        &page,
        Ok(result(
            &page,
            ViewResponse::QueuePage(Page {
                total: 1,
                rows: vec![QueueRow {
                    offset: 0,
                    ..queue_row(16, 202)
                }]
            })
        ))
    ));
    assert_eq!(tui.ui.queue.total, 1);
    assert_eq!(tui.snapshot.queue.total, 19);
    key(&mut tui.ui, &next, KeyCode::Esc);
    let clear = tui.ui.query_request(&next).unwrap();
    assert!(matches!(
        clear.demand.query(),
        Query::QueueEntries { query: None, queue_ids } if queue_ids == vec![202]
    ));
}

#[test]
fn playlist_query_and_occurrence_lookup_keep_playlist_scope_and_filtered_offset() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = playlist_view(1);
    submit(&mut tui.ui, &state, "song");
    let first = tui.ui.query_request(&state).unwrap();
    assert!(
        matches!(first.demand.query(), Query::PlaylistEntries { playlist_id: 1, query: Some(query), .. } if query == "song")
    );
    assert!(tui.ui.apply_query(&state, &first, Ok(response(&first, 2))));
    key(&mut tui.ui, &state, KeyCode::Down);
    assert!(matches!(
        key(&mut tui.ui, &state, KeyCode::Delete),
        KeyAction::Command(Command::RemovePlaylistEntry { entry_id: 1 })
    ));
    let mut next = state.clone();
    *Arc::make_mut(&mut next.library.playlist_entry_revisions)
        .get_mut(&1)
        .unwrap() += 1;
    tui.apply_snapshot(next.clone());
    let lookup = tui.ui.query_request(&next).unwrap();
    assert!(
        matches!(lookup.demand.query(), Query::PlaylistEntryLocations { playlist_id: 1, query: Some(query), entry_ids } if query == "song" && entry_ids == vec![1])
    );
    assert!(tui.ui.apply_query(
        &next,
        &lookup,
        Ok(result(
            &lookup,
            ViewResponse::PlaylistEntryLocations(vec![PlaylistEntryLocation {
                id: 1,
                playlist_id: 1,
                track_id: 7,
                position: 19,
                offset: 1,
                total: 2
            }])
        ))
    ));
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        panic!("playlist detail");
    };
    assert_eq!(detail.entries.selection.selected(), Some(1));
    assert_eq!(detail.entries.total, 2);
    key(&mut tui.ui, &next, KeyCode::Left);
    assert!(!tui.ui.search.active);
    assert!(matches!(tui.ui.view, View::Playlists(_)));
    assert!(matches!(
        tui.ui.query_request(&next).unwrap().demand.query(),
        Query::PlaylistSummaries { query: None, .. }
    ));
}

#[test]
fn directory_search_keeps_directory_semantics_and_clears_on_opening_target() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: View::Tree(LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        }),
        ..UiState::default()
    };
    submit(&mut ui, &state, "child");
    let request = ui.query_request(&state).unwrap();
    assert!(
        matches!(request.demand.query(), Query::DirectoryPage { path, query: Some(query), .. } if path == Path::new("/music") && query == "child")
    );
    assert!(ui.apply_query(
        &state,
        &request,
        Ok(result(
            &request,
            ViewResponse::DirectoryPage(Page {
                total: 1,
                rows: vec![DirectoryRow::Directory {
                    path: "/music/child".into()
                }]
            })
        ))
    ));
    key(&mut ui, &state, KeyCode::Right);
    assert!(!ui.search.active);
    assert!(
        matches!(ui.query_request(&state).unwrap().demand.query(), Query::DirectoryPage { path, query: None, offset: 0, .. } if path == Path::new("/music/child"))
    );
}

#[test]
fn directory_refresh_locates_selected_track_with_same_query() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = View::Tree(LibraryTree {
        directory: "/music".into(),
        ..LibraryTree::default()
    });
    submit(&mut tui.ui, &state, "song");
    let request = tui.ui.query_request(&state).unwrap();
    assert!(tui.ui.apply_query(
        &state,
        &request,
        Ok(result(
            &request,
            ViewResponse::DirectoryPage(Page {
                total: 2,
                rows: vec![
                    DirectoryRow::Track(track(42)),
                    DirectoryRow::Track(track(43))
                ]
            })
        ))
    ));
    let mut next = state.clone();
    next.library.revision += 1;
    tui.apply_snapshot(next.clone());
    let locate = tui.ui.query_request(&next).unwrap();
    assert!(
        matches!(locate.demand.query(), Query::LibraryLocate { track_id: 42, path, tree: true, query: Some(query), .. } if path == Path::new("/music") && query == "song")
    );
}

#[test]
fn other_uses_global_editor_and_retains_collection_filters_and_sort() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: View::Tree(LibraryTree {
            other_tracks: true,
            favorite: Some(true),
            missing: Some(false),
            sort: LibrarySort::Album,
            ..LibraryTree::default()
        }),
        ..UiState::default()
    };
    submit(&mut ui, &state, "outside");
    let request = ui.query_request(&state).unwrap();
    assert!(
        matches!(request.demand.query(), Query::OtherTracksPage { query: Some(query), favorite: Some(true), missing: Some(false), sort: LibrarySort::Album, .. } if query == "outside")
    );
    key(&mut ui, &state, KeyCode::Esc);
    let cleared = ui.query_request(&state).unwrap();
    assert!(
        matches!(cleared.demand.query(), Query::OtherTracksPage { query: Some(query), favorite: Some(true), missing: Some(false), sort: LibrarySort::Album, .. } if query.is_empty())
    );
}

#[test]
fn recent_filters_only_titles_locally_and_preserves_same_query_selection() {
    let mut state = query_snapshot();
    state.library.history = Arc::new(vec![
        HistoryEntry {
            track_id: 1,
            title: "Needle A".into(),
            play_count: 1,
            played_at: 1,
            favorite: false,
        },
        HistoryEntry {
            track_id: 2,
            title: "unrelated".into(),
            play_count: 99,
            played_at: 3,
            favorite: true,
        },
        HistoryEntry {
            track_id: 3,
            title: "needle B".into(),
            play_count: 1,
            played_at: 2,
            favorite: false,
        },
    ]);
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    let mut recent = RecentTracks::default();
    recent.sync(&state, "");
    tui.ui.view = View::Recent(recent);
    tui.ui.open_search();
    tui.ui.search.query = "NEEDLE".into();
    assert!(tui.ui.query_request(&state).is_none());
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.entries.total, 3);
    key(&mut tui.ui, &state, KeyCode::Enter);
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.entries.total, 2);
    key(&mut tui.ui, &state, KeyCode::Down);
    tui.ui.open_search();
    key(&mut tui.ui, &state, KeyCode::Enter);
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.selected().unwrap().track_id, 1);
    let mut next = state.clone();
    Arc::make_mut(&mut next.library.history)[0].played_at = 10;
    tui.apply_snapshot(next.clone());
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.selected().unwrap().track_id, 1);
    assert_eq!(recent.entries.selection.selected(), Some(0));
    assert!(matches!(
        key(&mut tui.ui, &next, KeyCode::Enter),
        KeyAction::Command(Command::Play { track_id: 1 })
    ));
    key(&mut tui.ui, &next, KeyCode::Down);
    tui.ui.open_search();
    tui.ui.search.query = "e".into();
    key(&mut tui.ui, &next, KeyCode::Enter);
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.selected().unwrap().track_id, 3);
    key(&mut tui.ui, &next, KeyCode::Down);
    key(&mut tui.ui, &next, KeyCode::Esc);
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.entries.total, 3);
    assert_eq!(recent.selected().unwrap().track_id, 3);
    assert_eq!(recent.entries.selection.selected(), Some(2));
    key(&mut tui.ui, &next, KeyCode::Down);
    tui.ui.open_search();
    key(&mut tui.ui, &next, KeyCode::Esc);
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.selected().unwrap().track_id, 3);
    assert_eq!(recent.entries.selection.selected(), Some(2));
    assert_eq!(recent.entries.selection.offset(), 2);
}

#[test]
fn tab_switching_from_editor_clears_search_and_never_caches_query() {
    for tab in [KeyCode::Tab, KeyCode::BackTab] {
        let (mut ui, state) = library_fixture();
        ui.open_search();
        ui.search.query = "draft".into();
        key(&mut ui, &state, tab);
        assert!(!ui.search.active);
        assert!(ui.search.query.is_empty());
        assert!(matches!(ui.cached_library, Some(View::Tree(_))));
        key(
            &mut ui,
            &state,
            if tab == KeyCode::Tab {
                KeyCode::BackTab
            } else {
                KeyCode::Tab
            },
        );
        assert!(!ui.search.active);
        assert!(matches!(ui.view, View::Tree(_)));
    }
}

#[test]
fn library_same_query_refresh_keeps_identity_and_rejects_old_revision() {
    let (ui, state) = library_fixture();
    let mut tui = TuiState {
        snapshot: state.clone(),
        ui,
    };
    key(&mut tui.ui, &state, KeyCode::Down);
    let old = tui.ui.query_request(&state).unwrap();
    let mut next = state.clone();
    next.library.revision += 1;
    tui.apply_snapshot(next.clone());
    assert!(!tui.ui.apply_query(&next, &old, Ok(response(&old, 12))));
    let locate = tui.ui.query_request(&next).unwrap();
    assert!(
        matches!(locate.demand.query(), Query::LibraryLocate { track_id: 1, query: Some(query), tree: false, .. } if query == "active")
    );
    let located = result(
        &locate,
        ViewResponse::LibraryLocate(LibraryLocateResult::Located(LibraryLocation {
            offset: 7,
            total: 12,
            child: None,
        })),
    );
    assert!(tui.ui.apply_query(&next, &locate, Ok(located)));
    let View::LibraryResults(rows) = &tui.ui.view else {
        panic!("library results");
    };
    assert_eq!(rows.selection.selected(), Some(7));
    assert_eq!(tui.ui.query_request(&next).unwrap().demand.offset, 6);
    // Another revision before the relocated page arrives must retain the ID,
    // rather than treating its new offset as an identity.
    next.library.revision += 1;
    tui.apply_snapshot(next.clone());
    assert!(matches!(
        tui.ui.query_request(&next).unwrap().demand.query(),
        Query::LibraryLocate { track_id: 1, .. }
    ));
}

#[test]
fn playlist_summary_search_keeps_enter_action_and_clears_on_expansion() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: View::Playlists(PagedList::default()),
        ..UiState::default()
    };
    submit(&mut ui, &state, "collection");
    let request = ui.query_request(&state).unwrap();
    assert!(
        matches!(request.demand.query(), Query::PlaylistSummaries { query: Some(query), offset: 0, .. } if query == "collection")
    );
    assert!(ui.apply_query(&state, &request, Ok(response(&request, 2))));
    assert!(matches!(
        key(&mut ui, &state, KeyCode::Enter),
        KeyAction::Ignored
    ));
    assert!(matches!(
        key(&mut ui, &state, KeyCode::Enter),
        KeyAction::Command(Command::PlayPlaylist { playlist_id: 0 })
    ));
    key(&mut ui, &state, KeyCode::Right);
    assert!(!ui.search.active);
    assert!(matches!(&ui.view, View::PlaylistDetail { detail, .. } if detail.playlist_id == 0));
}

#[test]
fn empty_queue_search_never_leaves_an_unloadable_result_page() {
    let state = TuiSnapshot::default();
    let mut ui = UiState::default();
    submit(&mut ui, &state, "missing");
    assert!(ui.query_request(&state).is_none());
    assert!(ui.queue.loaded);
    assert_eq!(ui.queue.total, 0);
    key(&mut ui, &state, KeyCode::Esc);
    assert!(!ui.search.active);
    assert!(ui.queue.loaded);
}

#[test]
fn playlist_metadata_filter_changes_relocate_the_selected_occurrence_before_refilling() {
    for navigate_after_locate in [false, true] {
        let state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
        tui.ui.view = playlist_view(1);
        submit(&mut tui.ui, &state, "song");
        let first = tui.ui.query_request(&state).unwrap();
        let mut initial = response(&first, 3);
        let ViewResponse::PlaylistEntries(page) = &mut initial.view else {
            panic!("playlist entries");
        };
        // B and C are separate occurrences of the same track.
        page.rows[1].track_id = 7;
        page.rows[2].track_id = 7;
        assert!(tui.ui.apply_query(&state, &first, Ok(initial)));
        key(&mut tui.ui, &state, KeyCode::Down);

        // A stops matching after a metadata edit; playlist membership/order did
        // not change, but B's filtered offset moved from 1 to 0.
        let mut next = state.clone();
        next.library.revision += 1;
        tui.apply_snapshot(next.clone());
        let stale = tui.ui.query_request(&next).unwrap();
        assert_eq!(
            stale.demand.query(),
            Query::PlaylistEntryLocations {
                playlist_id: 1,
                query: Some("song".into()),
                entry_ids: vec![1],
            }
        );
        // A second metadata publication must retain B's occurrence identity
        // even while the old pages have already been invalidated.
        next.library.revision += 1;
        tui.apply_snapshot(next.clone());
        assert_eq!(
            next.library.playlist_entry_revisions,
            state.library.playlist_entry_revisions
        );
        assert!(!tui.ui.apply_query(&next, &stale, Ok(response(&stale, 2))));
        let lookup = tui.ui.query_request(&next).unwrap();
        assert_eq!(lookup.demand.kind(), stale.demand.kind());
        let located = result(
            &lookup,
            ViewResponse::PlaylistEntryLocations(vec![PlaylistEntryLocation {
                id: 1,
                playlist_id: 1,
                track_id: 7,
                position: 1,
                offset: 0,
                total: 2,
            }]),
        );
        assert!(tui.ui.apply_query(&next, &lookup, Ok(located)));
        if navigate_after_locate {
            key(&mut tui.ui, &next, KeyCode::Down);
            // A late location result cannot override the newer navigation.
            assert!(!tui.ui.apply_query(&next, &lookup, Ok(response(&lookup, 2))));
        }
        let request = tui.ui.query_request(&next).unwrap();
        assert_eq!(request.demand.kind(), ReadKind::Playlist(1));
        assert_eq!(request.demand.offset, 0);
        let mut refreshed = response(&request, 2);
        let ViewResponse::PlaylistEntries(page) = &mut refreshed.view else {
            panic!("playlist entries");
        };
        for entry in &mut page.rows {
            entry.id += 1;
            entry.position += 1;
            entry.track_id = 7;
        }
        assert!(tui.ui.apply_query(&next, &request, Ok(refreshed)));
        let expected_id = if navigate_after_locate { 2 } else { 1 };
        let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
            panic!("playlist detail");
        };
        assert_eq!(detail.entries.total, 2);
        assert_eq!(detail.entries.selected().unwrap().id, expected_id);
        assert_eq!(detail.selected_entry_id, Some(expected_id));
        assert!(matches!(
            key(&mut tui.ui, &next, KeyCode::Delete),
            KeyAction::Command(Command::RemovePlaylistEntry { entry_id }) if entry_id == expected_id
        ));
    }
}

fn replace_query(ui: &mut UiState, state: &TuiSnapshot, query: &str) {
    ui.open_search();
    ui.search.query = query.into();
    assert!(matches!(key(ui, state, KeyCode::Enter), KeyAction::Ignored));
}

#[test]
fn track_search_changes_locate_unloaded_identity_and_same_query_keeps_pages() {
    for kind in 0..3 {
        let mut state = query_snapshot();
        state.system.query_batch_size = 2;
        let mut ui = UiState {
            view: match kind {
                0 => View::Tree(LibraryTree::new(&state.system.library_roots)),
                1 => View::Tree(LibraryTree {
                    directory: "/music".into(),
                    ..LibraryTree::default()
                }),
                _ => View::Tree(LibraryTree {
                    other_tracks: true,
                    ..LibraryTree::default()
                }),
            },
            ..UiState::default()
        };
        replace_query(&mut ui, &state, "first");
        let initial = ui.query_request(&state).unwrap();
        assert!(ui.apply_query(&state, &initial, Ok(response(&initial, 12))));
        key(&mut ui, &state, KeyCode::Down);
        replace_query(&mut ui, &state, "second");
        let lookup = ui.query_request(&state).unwrap();
        assert!(matches!(
            lookup.demand.query(),
            Query::LibraryLocate { track_id: 1, .. } | Query::OtherTracksLocate { track_id: 1, .. }
        ));
        let located = result(
            &lookup,
            ViewResponse::LibraryLocate(LibraryLocateResult::Located(LibraryLocation {
                offset: 7,
                total: 12,
                child: None,
            })),
        );
        assert!(ui.apply_query(&state, &lookup, Ok(located)));
        let page = ui.query_request(&state).unwrap();
        assert_eq!(page.demand.offset, 6);
        let mut loaded = response(&page, 12);
        match &mut loaded.view {
            ViewResponse::LibraryPage(page) => page.rows[1].id = 1,
            ViewResponse::DirectoryPage(page) => page.rows[1] = DirectoryRow::Track(track(1)),
            _ => panic!("track page"),
        }
        assert!(ui.apply_query(&state, &page, Ok(loaded)));
        let demand = ui.query_request(&state);
        let (selection, scroll, allocation) = match &ui.view {
            View::LibraryResults(rows) => (
                rows.selection.selected(),
                rows.selection.offset(),
                rows.pages[&6].as_ptr() as usize,
            ),
            View::Tree(tree) => (
                tree.entries.selection.selected(),
                tree.entries.selection.offset(),
                tree.entries.pages[&6].as_ptr() as usize,
            ),
            _ => unreachable!(),
        };
        replace_query(&mut ui, &state, " second ");
        assert_eq!(ui.query_request(&state), demand);
        match &ui.view {
            View::LibraryResults(rows) => {
                assert_eq!(rows.selection.selected(), selection);
                assert_eq!(rows.selection.offset(), scroll);
                assert_eq!(rows.pages[&6].as_ptr() as usize, allocation);
            }
            View::Tree(tree) => {
                assert_eq!(tree.entries.selection.selected(), selection);
                assert_eq!(tree.entries.selection.offset(), scroll);
                assert_eq!(tree.entries.pages[&6].as_ptr() as usize, allocation);
            }
            _ => unreachable!(),
        }
        if kind != 0 {
            key(&mut ui, &state, KeyCode::Esc);
            let clear = ui.query_request(&state).unwrap();
            assert!(matches!(
                clear.demand.query(),
                Query::LibraryLocate {
                    track_id: 1,
                    query: None,
                    ..
                } | Query::OtherTracksLocate {
                    track_id: 1,
                    query: Some(_),
                    ..
                }
            ));
        }
    }
}

#[test]
fn queue_search_changes_and_tab_clear_preserve_duplicate_occurrence() {
    let mut state = query_snapshot();
    state.queue.total = 20;
    state.system.query_batch_size = 2;
    let mut ui = UiState::default();
    replace_query(&mut ui, &state, "first");
    let first = ui.query_request(&state).unwrap();
    let mut initial = response(&first, 4);
    let ViewResponse::QueuePage(page) = &mut initial.view else {
        unreachable!()
    };
    page.rows[0].track.id = 7;
    page.rows[1].track.id = 7;
    assert!(ui.apply_query(&state, &first, Ok(initial)));
    key(&mut ui, &state, KeyCode::Down);
    replace_query(&mut ui, &state, "second");
    let lookup = ui.query_request(&state).unwrap();
    assert!(
        matches!(lookup.demand.query(), Query::QueueEntries { queue_ids, query: Some(query) } if queue_ids == vec![1001] && query == "second")
    );
    let mut selected = queue_row(17, 1001);
    selected.offset = 5;
    assert!(ui.apply_query(
        &state,
        &lookup,
        Ok(result(&lookup, ViewResponse::QueueEntries(vec![selected])))
    ));
    let page = ui.query_request(&state).unwrap();
    assert_eq!(page.demand.offset, 4);
    let mut loaded = response(&page, 6);
    let ViewResponse::QueuePage(rows) = &mut loaded.view else {
        unreachable!()
    };
    rows.rows[1].id = 1001;
    rows.rows[1].track.id = 7;
    assert!(ui.apply_query(&state, &page, Ok(loaded)));
    let demand = ui.query_request(&state);
    replace_query(&mut ui, &state, "second");
    assert_eq!(ui.query_request(&state), demand);
    assert!(matches!(
        key(&mut ui, &state, KeyCode::Enter),
        KeyAction::Command(Command::PlayQueue { queue_id: 1001 })
    ));
    key(&mut ui, &state, KeyCode::Tab);
    assert!(!ui.search.active);
    key(&mut ui, &state, KeyCode::BackTab);
    let clear = ui.query_request(&state).unwrap();
    assert!(
        matches!(clear.demand.query(), Query::QueueEntries { queue_ids, query: None } if queue_ids == vec![1001])
    );
    assert!(!ui.apply_query(
        &state,
        &lookup,
        Ok(result(&lookup, ViewResponse::QueueEntries(vec![])))
    ));
}

#[test]
fn playlist_search_changes_and_tab_clear_preserve_duplicate_entry() {
    let mut state = query_snapshot();
    state.system.query_batch_size = 2;
    let mut ui = UiState {
        view: playlist_view(1),
        ..UiState::default()
    };
    replace_query(&mut ui, &state, "first");
    let first = ui.query_request(&state).unwrap();
    let mut initial = response(&first, 8);
    let ViewResponse::PlaylistEntries(page) = &mut initial.view else {
        unreachable!()
    };
    for entry in &mut page.rows {
        entry.track_id = 7;
    }
    assert!(ui.apply_query(&state, &first, Ok(initial)));
    key(&mut ui, &state, KeyCode::Down);
    replace_query(&mut ui, &state, "second");
    let lookup = ui.query_request(&state).unwrap();
    assert!(
        matches!(lookup.demand.query(), Query::PlaylistEntryLocations { entry_ids, .. } if entry_ids == vec![1])
    );
    let location = PlaylistEntryLocation {
        id: 1,
        playlist_id: 1,
        track_id: 7,
        position: 19,
        offset: 7,
        total: 8,
    };
    assert!(ui.apply_query(
        &state,
        &lookup,
        Ok(result(
            &lookup,
            ViewResponse::PlaylistEntryLocations(vec![location])
        ))
    ));
    let page = ui.query_request(&state).unwrap();
    assert_eq!(page.demand.offset, 6);
    let mut loaded = response(&page, 8);
    let ViewResponse::PlaylistEntries(rows) = &mut loaded.view else {
        unreachable!()
    };
    rows.rows[1].id = 1;
    rows.rows[1].track_id = 7;
    assert!(ui.apply_query(&state, &page, Ok(loaded)));
    let demand = ui.query_request(&state);
    replace_query(&mut ui, &state, "second");
    assert_eq!(ui.query_request(&state), demand);
    assert!(matches!(
        key(&mut ui, &state, KeyCode::Delete),
        KeyAction::Command(Command::RemovePlaylistEntry { entry_id: 1 })
    ));
    key(&mut ui, &state, KeyCode::Tab);
    key(&mut ui, &state, KeyCode::BackTab);
    let clear = ui.query_request(&state).unwrap();
    assert!(
        matches!(clear.demand.query(), Query::PlaylistEntryLocations { entry_ids, query: None, .. } if entry_ids == vec![1])
    );
}

#[test]
fn missing_search_identity_falls_back_once_without_ghost_targets() {
    for kind in 0..5 {
        for total in [0, 2] {
            let mut state = query_snapshot();
            state.queue.total = 20;
            state.system.query_batch_size = 2;
            let ui = UiState {
                view: match kind {
                    0 => View::Queue,
                    1 => View::Tree(LibraryTree::new(&state.system.library_roots)),
                    2 => View::Tree(LibraryTree {
                        directory: "/music".into(),
                        ..LibraryTree::default()
                    }),
                    3 => View::Tree(LibraryTree {
                        other_tracks: true,
                        ..LibraryTree::default()
                    }),
                    _ => playlist_view(1),
                },
                ..UiState::default()
            };
            let mut tui = TuiState {
                snapshot: state.clone(),
                ui,
            };
            replace_query(&mut tui.ui, &state, "first");
            let initial = tui.ui.query_request(&state).unwrap();
            assert!(
                tui.ui
                    .apply_query(&state, &initial, Ok(response(&initial, 4)))
            );
            key(&mut tui.ui, &state, KeyCode::Down);
            replace_query(&mut tui.ui, &state, "second");
            let lookup = tui.ui.query_request(&state).unwrap();
            let missing = match kind {
                0 => ViewResponse::QueueEntries(vec![]),
                4 => ViewResponse::PlaylistEntryLocations(vec![]),
                _ => ViewResponse::LibraryLocate(LibraryLocateResult::Filtered),
            };
            assert!(
                tui.ui
                    .apply_query(&state, &lookup, Ok(result(&lookup, missing)))
            );
            let fallback = tui.ui.query_request(&state).unwrap();
            assert_eq!(fallback.demand.offset, 0);
            // Publish again before the fallback page: the disappeared target
            // must not be resurrected from remembered identity fields.
            state.queue.revision += 1;
            state.library.revision += 1;
            *Arc::make_mut(&mut state.library.playlist_entry_revisions)
                .get_mut(&1)
                .unwrap() += 1;
            tui.apply_snapshot(state.clone());
            let fresh = tui.ui.query_request(&state).unwrap();
            assert!(matches!(
                fresh.demand.query(),
                Query::QueuePage { .. }
                    | Query::LibraryPage { .. }
                    | Query::DirectoryPage { .. }
                    | Query::OtherTracksPage { .. }
                    | Query::PlaylistEntries { .. }
            ));
            assert_eq!(fresh.demand.offset, 0);
            assert!(
                tui.ui
                    .apply_query(&state, &fresh, Ok(response(&fresh, total)))
            );
            let selected = match &tui.ui.view {
                View::Queue => tui.ui.queue.selection.selected(),
                View::LibraryResults(rows) => rows.selection.selected(),
                View::Tree(tree) => tree.entries.selection.selected(),
                View::PlaylistDetail { detail, .. } => detail.entries.selection.selected(),
                _ => unreachable!(),
            };
            assert_eq!(selected, (total != 0).then_some(0));
        }
    }
}

fn container_fixture(catalog: bool) -> (UiState, TuiSnapshot) {
    let mut state = query_snapshot();
    state.system.query_batch_size = 2;
    let view = if catalog {
        let mut rows = PagedList::default();
        rows.insert(
            0,
            2,
            2,
            vec![
                PlaylistSummary {
                    id: 0,
                    name: "first".into(),
                    entry_count: 0,
                },
                PlaylistSummary {
                    id: 1,
                    name: "target".into(),
                    entry_count: 0,
                },
            ],
        );
        rows.selection.select(Some(1));
        View::Playlists(rows)
    } else {
        let mut tree = LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        };
        tree.entries.insert(
            0,
            2,
            2,
            vec![
                DirectoryRow::Directory {
                    path: "/music/first".into(),
                },
                DirectoryRow::Directory {
                    path: "/music/target".into(),
                },
            ],
        );
        tree.entries.selection.select(Some(1));
        View::Tree(tree)
    };
    (
        UiState {
            view,
            ..UiState::default()
        },
        state,
    )
}

fn container_page(request: &QueryRequest, catalog: bool, found: bool, total: usize) -> QueryResult {
    let indices = request.demand.offset..(request.demand.offset + request.demand.limit).min(total);
    let view = if catalog {
        ViewResponse::PlaylistSummaries(Page {
            total,
            rows: indices
                .map(|index| PlaylistSummary {
                    id: if found && index == 3 {
                        1
                    } else {
                        index as i64 + 10
                    },
                    name: format!("row-{index}"),
                    entry_count: 0,
                })
                .collect(),
        })
    } else {
        ViewResponse::DirectoryPage(Page {
            total,
            rows: indices
                .map(|index| DirectoryRow::Directory {
                    path: if found && index == 3 {
                        "/music/target".into()
                    } else {
                        format!("/music/{index}").into()
                    },
                })
                .collect(),
        })
    };
    result(request, view)
}

#[test]
fn container_revision_refresh_preserves_identity_and_activation_in_visible_and_hidden_views() {
    for catalog in [false, true] {
        for hidden in [false, true] {
            let (mut ui, mut snapshot) = container_fixture(catalog);
            snapshot.system.query_batch_size = 3;
            if let View::Playlists(rows) = &mut ui.view {
                rows.insert(
                    0,
                    3,
                    3,
                    [0, 1, 2]
                        .map(|id| PlaylistSummary {
                            id,
                            name: format!("playlist-{id}"),
                            entry_count: 0,
                        })
                        .into(),
                );
            }
            if hidden {
                let view = std::mem::replace(&mut ui.view, View::Queue);
                if catalog {
                    ui.cached_playlists = Some(view);
                } else {
                    ui.cached_library = Some(view);
                }
            }
            let mut tui = TuiState { snapshot, ui };
            let mut next = tui.snapshot.clone();
            next.library.revision += 1;
            next.library.structure_revision += 1;
            next.library.playlist_revision += 1;
            tui.apply_snapshot(next.clone());
            if hidden {
                tui.ui.view = if catalog {
                    tui.ui.cached_playlists.take().unwrap()
                } else {
                    tui.ui.cached_library.take().unwrap()
                };
            }
            let request = tui.ui.query_request(&next).unwrap();
            assert_eq!(request.demand.offset, 0);
            let page = if catalog {
                ViewResponse::PlaylistSummaries(Page {
                    total: 3,
                    rows: [2, 0, 1]
                        .map(|id| PlaylistSummary {
                            id,
                            name: format!("playlist-{id}"),
                            entry_count: 0,
                        })
                        .into(),
                })
            } else {
                ViewResponse::DirectoryPage(Page {
                    total: 3,
                    rows: ["new", "first", "target"]
                        .map(|name| DirectoryRow::Directory {
                            path: format!("/music/{name}").into(),
                        })
                        .into(),
                })
            };
            assert!(
                tui.ui
                    .apply_query(&next, &request, Ok(result(&request, page)))
            );
            match &tui.ui.view {
                View::Playlists(rows) => {
                    assert_eq!(rows.selection.selected(), Some(2));
                    assert_eq!(rows.selected().unwrap().id, 1);
                }
                View::Tree(tree) => {
                    assert_eq!(tree.entries.selection.selected(), Some(2));
                    assert!(
                        matches!(tree.entries.selected(), Some(DirectoryRow::Directory { path }) if path == Path::new("/music/target"))
                    );
                }
                _ => unreachable!(),
            }
            if catalog {
                assert!(matches!(
                    key(&mut tui.ui, &next, KeyCode::Enter),
                    KeyAction::Ignored
                ));
                assert!(matches!(
                    key(&mut tui.ui, &next, KeyCode::Enter),
                    KeyAction::Command(Command::PlayPlaylist { playlist_id: 1 })
                ));
                key(&mut tui.ui, &next, KeyCode::Right);
                assert!(
                    matches!(&tui.ui.view, View::PlaylistDetail { detail, .. } if detail.playlist_id == 1)
                );
            } else {
                key(&mut tui.ui, &next, KeyCode::Enter);
                assert!(
                    matches!(&tui.ui.view, View::Tree(tree) if tree.directory == Path::new("/music/target"))
                );
            }
        }
    }
}

#[test]
fn container_revision_refresh_restarts_pending_identity_scan() {
    for catalog in [false, true] {
        let (mut ui, snapshot) = container_fixture(catalog);
        replace_query(&mut ui, &snapshot, "filter");
        let first = ui.query_request(&snapshot).unwrap();
        assert!(ui.apply_query(
            &snapshot,
            &first,
            Ok(container_page(&first, catalog, true, 4))
        ));
        let obsolete = ui.query_request(&snapshot).unwrap();
        assert_eq!(obsolete.demand.offset, 2);
        let mut tui = TuiState { snapshot, ui };
        let mut next = tui.snapshot.clone();
        next.library.revision += 1;
        next.library.structure_revision += 1;
        next.library.playlist_revision += 1;
        tui.apply_snapshot(next.clone());
        assert!(!tui.ui.apply_query(
            &next,
            &obsolete,
            Ok(container_page(&obsolete, catalog, true, 4))
        ));
        for offset in [0, 2] {
            let request = tui.ui.query_request(&next).unwrap();
            assert_eq!(request.demand.offset, offset);
            assert!(tui.ui.apply_query(
                &next,
                &request,
                Ok(container_page(&request, catalog, true, 4))
            ));
        }
        match &tui.ui.view {
            View::Playlists(rows) => assert_eq!(rows.selected().unwrap().id, 1),
            View::Tree(tree) => assert!(
                matches!(tree.entries.selected(), Some(DirectoryRow::Directory { path }) if path == Path::new("/music/target"))
            ),
            _ => unreachable!(),
        }
    }
}

#[test]
fn container_search_scans_bounded_pages_and_finishes_found_missing_or_empty() {
    for catalog in [false, true] {
        for (found, total) in [(true, 4), (false, 4), (false, 0)] {
            let (mut ui, state) = container_fixture(catalog);
            replace_query(&mut ui, &state, "filter");
            let first = ui.query_request(&state).unwrap();
            assert_eq!(first.demand.offset, 0);
            assert!(ui.apply_query(
                &state,
                &first,
                Ok(container_page(&first, catalog, found, total))
            ));
            if total != 0 {
                match &ui.view {
                    View::Tree(tree) => {
                        assert_eq!(tree.entries.pages.len(), 1);
                        assert!(tree.entries.pages.contains_key(&0));
                    }
                    View::Playlists(rows) => {
                        assert_eq!(rows.pages.len(), 1);
                        assert!(rows.pages.contains_key(&0));
                    }
                    _ => unreachable!(),
                }
                let second = ui.query_request(&state).unwrap();
                assert_eq!(second.demand.offset, 2);
                assert!(!ui.apply_query(
                    &state,
                    &first,
                    Ok(container_page(&first, catalog, found, total))
                ));
                assert!(ui.apply_query(
                    &state,
                    &second,
                    Ok(container_page(&second, catalog, found, total))
                ));
            }
            let (selection, pending) = match &ui.view {
                View::Tree(tree) => (
                    tree.entries.selection.selected(),
                    tree.entries.identity_lookup.is_some(),
                ),
                View::Playlists(rows) => {
                    (rows.selection.selected(), rows.identity_lookup.is_some())
                }
                _ => unreachable!(),
            };
            assert!(!pending);
            assert_eq!(
                selection,
                if total == 0 {
                    None
                } else if found {
                    Some(3)
                } else {
                    Some(0)
                }
            );
            let demand = ui.query_request(&state);
            replace_query(&mut ui, &state, "filter");
            assert_eq!(ui.query_request(&state), demand);
            let mut tui = TuiState {
                snapshot: state.clone(),
                ui,
            };
            for _ in 0..2 {
                let mut next = tui.snapshot.clone();
                next.library.revision += 1;
                next.library.structure_revision += 1;
                next.library.playlist_revision += 1;
                tui.apply_snapshot(next.clone());
                for _ in 0..2 {
                    let page = tui.ui.query_request(&next).unwrap();
                    assert!(tui.ui.apply_query(
                        &next,
                        &page,
                        Ok(container_page(&page, catalog, false, 4))
                    ));
                    let pending = match &tui.ui.view {
                        View::Tree(tree) => tree.entries.identity_lookup.is_some(),
                        View::Playlists(rows) => rows.identity_lookup.is_some(),
                        _ => unreachable!(),
                    };
                    if !pending {
                        break;
                    }
                }
                match &tui.ui.view {
                    View::Tree(tree) => {
                        assert!(tree.entries.identity_lookup.is_none());
                        assert!(
                            matches!(tree.entries.selected(), Some(DirectoryRow::Directory { path }) if path != Path::new("/music/target"))
                        );
                    }
                    View::Playlists(rows) => {
                        assert!(rows.identity_lookup.is_none());
                        assert_ne!(rows.selected().unwrap().id, 1);
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

#[test]
fn container_scan_clear_survives_tab_cache_and_navigation_rejects_late_pages() {
    for catalog in [false, true] {
        let (mut ui, state) = container_fixture(catalog);
        replace_query(&mut ui, &state, "filter");
        let first = ui.query_request(&state).unwrap();
        assert!(ui.apply_query(&state, &first, Ok(container_page(&first, catalog, true, 4))));
        let old = ui.query_request(&state).unwrap();
        assert_eq!(old.demand.offset, 2);
        key(&mut ui, &state, KeyCode::Tab);
        assert!(!ui.apply_query(&state, &old, Ok(container_page(&old, catalog, true, 4))));
        key(&mut ui, &state, KeyCode::BackTab);
        let clear = ui.query_request(&state).unwrap();
        assert_eq!(clear.demand.borrowed().filter, None);
        assert_eq!(clear.demand.offset, 0);
        assert!(ui.apply_query(&state, &clear, Ok(container_page(&clear, catalog, true, 4))));
        let second = ui.query_request(&state).unwrap();
        assert!(ui.apply_query(
            &state,
            &second,
            Ok(container_page(&second, catalog, true, 4))
        ));
        match &ui.view {
            View::Tree(tree) => assert_eq!(tree.entries.selection.selected(), Some(3)),
            View::Playlists(rows) => assert_eq!(rows.selection.selected(), Some(3)),
            _ => unreachable!(),
        }
        replace_query(&mut ui, &state, "next");
        let initial = ui.query_request(&state).unwrap();
        assert!(ui.apply_query(
            &state,
            &initial,
            Ok(container_page(&initial, catalog, true, 4))
        ));
        let obsolete = ui.query_request(&state).unwrap();
        key(&mut ui, &state, KeyCode::Down);
        let fresh = ui.query_request(&state).unwrap();
        assert!(!ui.apply_query(
            &state,
            &obsolete,
            Ok(container_page(&obsolete, catalog, true, 4))
        ));
        assert!(ui.apply_query(&state, &fresh, Ok(container_page(&fresh, catalog, true, 4))));
        match &ui.view {
            View::Tree(tree) => {
                assert_eq!(tree.entries.selection.selected(), Some(1));
                assert!(tree.entries.identity_lookup.is_none());
            }
            View::Playlists(rows) => {
                assert_eq!(rows.selection.selected(), Some(1));
                assert!(rows.identity_lookup.is_none());
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn root_global_search_and_clear_never_restore_an_unreachable_container() {
    let mut state = query_snapshot();
    state.system.library_roots = Arc::new(vec!["/a".into(), "/b".into()]);
    let mut tree = LibraryTree::new(&state.system.library_roots);
    tree.other_present = Some(true);
    let mut ui = UiState {
        view: View::Tree(tree),
        ..UiState::default()
    };
    ui.prepare_local_view(&state);
    key(&mut ui, &state, KeyCode::Down);
    replace_query(&mut ui, &state, "song");
    let first = ui.query_request(&state).unwrap();
    assert!(matches!(
        first.demand.query(),
        Query::LibraryPage { offset: 0, .. }
    ));
    assert!(ui.apply_query(&state, &first, Ok(response(&first, 4))));
    key(&mut ui, &state, KeyCode::Down);
    key(&mut ui, &state, KeyCode::Esc);
    let count = ui.query_request(&state).unwrap();
    assert!(ui.apply_query(
        &state,
        &count,
        Ok(result(&count, ViewResponse::OtherTracksCount(1)))
    ));
    let View::Tree(tree) = &ui.view else {
        unreachable!()
    };
    assert_eq!(tree.entries.selection.selected(), Some(0));
    assert!(
        matches!(tree.entries.selected(), Some(DirectoryRow::Directory { path }) if path == Path::new("/a"))
    );
}

#[test]
fn navigation_cancels_pending_track_and_occurrence_search_locates() {
    for kind in 0..5 {
        let mut state = query_snapshot();
        state.system.query_batch_size = 2;
        let mut ui = UiState {
            view: match kind {
                0 => View::Queue,
                1 => View::Tree(LibraryTree::new(&state.system.library_roots)),
                2 => View::Tree(LibraryTree {
                    directory: "/music".into(),
                    ..LibraryTree::default()
                }),
                3 => View::Tree(LibraryTree {
                    other_tracks: true,
                    ..LibraryTree::default()
                }),
                _ => playlist_view(1),
            },
            ..UiState::default()
        };
        replace_query(&mut ui, &state, "first");
        let initial = ui.query_request(&state).unwrap();
        assert!(ui.apply_query(&state, &initial, Ok(response(&initial, 4))));
        key(&mut ui, &state, KeyCode::Down);
        replace_query(&mut ui, &state, "second");
        let obsolete = ui.query_request(&state).unwrap();
        key(&mut ui, &state, KeyCode::Down);
        let fresh = ui.query_request(&state).unwrap();
        let late = match kind {
            0 => {
                let mut row = queue_row(7, 1001);
                row.offset = 7;
                ViewResponse::QueueEntries(vec![row])
            }
            4 => ViewResponse::PlaylistEntryLocations(vec![PlaylistEntryLocation {
                id: 1,
                playlist_id: 1,
                track_id: 7,
                position: 7,
                offset: 7,
                total: 8,
            }]),
            _ => ViewResponse::LibraryLocate(LibraryLocateResult::Located(LibraryLocation {
                offset: 7,
                total: 8,
                child: None,
            })),
        };
        assert!(!ui.apply_query(&state, &obsolete, Ok(result(&obsolete, late))));
        assert_eq!(fresh.demand.offset, 0);
        assert!(ui.apply_query(&state, &fresh, Ok(response(&fresh, 4))));
        let selection = match &ui.view {
            View::Queue => ui.queue.selection.selected(),
            View::LibraryResults(rows) => rows.selection.selected(),
            View::Tree(tree) => tree.entries.selection.selected(),
            View::PlaylistDetail { detail, .. } => detail.entries.selection.selected(),
            _ => unreachable!(),
        };
        assert_eq!(selection, Some(0));
    }
}

#[test]
fn recent_search_preserves_current_track_or_first_without_hidden_selection() {
    let mut state = query_snapshot();
    state.library.history = Arc::new(
        (1..=3)
            .map(|id| HistoryEntry {
                track_id: id,
                title: if id == 2 {
                    "beta".into()
                } else {
                    "alpha".into()
                },
                play_count: 1,
                played_at: id,
                favorite: false,
            })
            .collect(),
    );
    let mut recent = RecentTracks::default();
    recent.sync(&state, "");
    let mut ui = UiState {
        view: View::Recent(recent),
        ..UiState::default()
    };
    key(&mut ui, &state, KeyCode::Down); // beta / track 2
    replace_query(&mut ui, &state, "alpha");
    let View::Recent(recent) = &ui.view else {
        unreachable!()
    };
    assert_eq!(recent.selected().unwrap().track_id, 3);
    key(&mut ui, &state, KeyCode::Down); // alpha / track 1
    let View::Recent(recent) = &ui.view else {
        unreachable!()
    };
    let allocation = recent.entries.pages[&0].as_ptr();
    replace_query(&mut ui, &state, "alpha");
    let View::Recent(recent) = &ui.view else {
        unreachable!()
    };
    assert_eq!(recent.entries.pages[&0].as_ptr(), allocation);
    assert_eq!(recent.selected().unwrap().track_id, 1);
    replace_query(&mut ui, &state, " ");
    let View::Recent(recent) = &ui.view else {
        unreachable!()
    };
    assert_eq!(recent.entries.total, 3);
    assert_eq!(recent.selected().unwrap().track_id, 1);
    assert!(!ui.search.active);
    replace_query(&mut ui, &state, "missing");
    let View::Recent(recent) = &ui.view else {
        unreachable!()
    };
    assert_eq!(recent.entries.total, 0);
    assert_eq!(recent.entries.selection.selected(), None);
    replace_query(&mut ui, &state, "alpha");
    let View::Recent(recent) = &ui.view else {
        unreachable!()
    };
    assert_eq!(recent.selected().unwrap().track_id, 3);
}

#[test]
fn container_scan_restores_scroll_and_discards_nonvisible_scan_batches() {
    let (mut ui, state) = container_fixture(false);
    let View::Tree(tree) = &mut ui.view else {
        unreachable!()
    };
    tree.entries.clear();
    tree.entries.viewport_rows = 4;
    tree.entries.selection = ListState::default().with_selected(Some(8)).with_offset(7);
    tree.entries.insert(
        8,
        2,
        20,
        vec![
            DirectoryRow::Directory {
                path: "/music/target".into(),
            },
            DirectoryRow::Directory {
                path: "/music/neighbor".into(),
            },
        ],
    );
    replace_query(&mut ui, &state, "filter");
    for expected in (0..=8).step_by(2) {
        let request = ui.query_request(&state).unwrap();
        assert_eq!(request.demand.offset, expected);
        let page = ViewResponse::DirectoryPage(Page {
            total: 20,
            rows: (expected..expected + 2)
                .map(|index| DirectoryRow::Directory {
                    path: if index == 9 {
                        "/music/target".into()
                    } else {
                        format!("/music/{index}").into()
                    },
                })
                .collect(),
        });
        assert!(ui.apply_query(&state, &request, Ok(result(&request, page))));
        let View::Tree(tree) = &ui.view else {
            unreachable!()
        };
        assert_eq!(tree.entries.pages.len(), 1);
    }
    let View::Tree(tree) = &ui.view else {
        unreachable!()
    };
    assert_eq!(tree.entries.selection.selected(), Some(9));
    assert_eq!(tree.entries.selection.offset(), 7);
    assert!(tree.entries.pages.contains_key(&8));
    assert!(tree.entries.identity_lookup.is_none());
}

#[test]
fn entry_navigation_and_search_do_not_reopen_failed_title_demand() {
    let mut state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = playlist_view(1);
    super::tests::load_visible(&mut tui.ui, &state);
    state.library.playlist_revision += 1;
    tui.apply_snapshot(state.clone());
    let title = tui.ui.query_request(&state).unwrap();
    assert_eq!(title.demand.kind(), ReadKind::PlaylistSummary(1));
    assert!(tui.ui.apply_query(
        &state,
        &title,
        Err(QueryError::Message("title unavailable".into()))
    ));

    for (navigation, offsets, track_id) in [(KeyCode::End, [9, 6], 11), (KeyCode::Home, [0, 3], 0)]
    {
        key(&mut tui.ui, &state, navigation);
        for offset in offsets {
            let page = tui.ui.query_request(&state).expect("navigated entry page");
            assert_eq!(page.demand.kind(), ReadKind::Playlist(1));
            assert_eq!(page.demand.offset, offset);
            assert!(tui.ui.apply_query(&state, &page, Ok(response(&page, 12))));
        }
        assert!(tui.ui.query_request(&state).is_none());
        assert!(matches!(
            key(&mut tui.ui, &state, KeyCode::Enter),
            KeyAction::Command(Command::Play { track_id: id }) if id == track_id
        ));
    }

    submit(&mut tui.ui, &state, "song");
    let lookup = tui.ui.query_request(&state).unwrap();
    assert_eq!(
        lookup.demand.kind(),
        ReadKind::PlaylistSelection {
            playlist_id: 1,
            entry_id: 0
        }
    );
    // Explicit navigation cancels the entry locate, not title suppression.
    key(&mut tui.ui, &state, KeyCode::Down);
    let filtered = tui.ui.query_request(&state).unwrap();
    assert_eq!(filtered.demand.kind(), ReadKind::Playlist(1));
    assert!(
        matches!(filtered.demand.query(), Query::PlaylistEntries { query: Some(query), .. } if query == "song")
    );
    assert!(
        !tui.ui
            .apply_query(&state, &lookup, Ok(response(&lookup, 2)))
    );
    assert_eq!(tui.ui.reads.requested.as_ref(), Some(&filtered));
    assert!(
        tui.ui
            .apply_query(&state, &filtered, Ok(response(&filtered, 2)))
    );
    assert!(tui.ui.query_request(&state).is_none());

    key(&mut tui.ui, &state, KeyCode::Esc);
    assert!(!tui.ui.search.active);
    let lookup = tui.ui.query_request(&state).unwrap();
    assert_eq!(
        lookup.demand.kind(),
        ReadKind::PlaylistSelection {
            playlist_id: 1,
            entry_id: 0
        }
    );
    assert!(matches!(
        lookup.demand.query(),
        Query::PlaylistEntryLocations { query: None, .. }
    ));
    assert!(
        tui.ui
            .apply_query(&state, &lookup, Ok(response(&lookup, 12)))
    );
    for offset in [0, 3] {
        let page = tui.ui.query_request(&state).unwrap();
        assert_eq!(page.demand.kind(), ReadKind::Playlist(1));
        assert_eq!(page.demand.offset, offset);
        assert!(tui.ui.apply_query(&state, &page, Ok(response(&page, 12))));
    }
    assert!(tui.ui.query_request(&state).is_none());
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        panic!("playlist detail");
    };
    assert_eq!(detail.entries.selected().unwrap().track_id, 0);
    assert_eq!(detail.name, "collection");
    assert!(detail.summary_dirty);
}
