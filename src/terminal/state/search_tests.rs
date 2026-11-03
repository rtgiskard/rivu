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
        revisions: request.stamp.revisions(),
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
    assert_eq!(fresh.offset, 0);
    assert!(
        matches!(fresh.query(), Query::LibraryPage { query: Some(query), .. } if query == "next")
    );
    let View::LibraryResults(rows) = &ui.view else {
        panic!("library results");
    };
    assert!(rows.pages.is_empty());
    assert!(ui.apply_query(&state, &fresh, Ok(response(&fresh, 12))));
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
    let fresh = ui.query_request(&state).unwrap();
    assert_eq!(fresh.query(), old.query());
    assert_ne!(fresh.generation, old.generation);
    assert!(!ui.apply_query(&state, &old, Ok(response(&old, 12))));
    key(&mut ui, &state, KeyCode::Esc);
    assert!(!ui.search.active);
    assert!(matches!(ui.view, View::Tree(_)));
    assert!(!ui.apply_query(&state, &fresh, Ok(response(&fresh, 12))));
}

#[test]
fn empty_draft_waits_for_enter_and_then_restores_unfiltered_page_first_row() {
    let (mut ui, state) = library_fixture();
    ui.open_search();
    ui.search.query = "  ".into();
    assert!(
        matches!(ui.query_request(&state).unwrap().query(), Query::LibraryPage { query: Some(query), .. } if query == "active")
    );
    key(&mut ui, &state, KeyCode::Enter);
    assert!(!ui.search.active);
    assert!(
        matches!(&ui.view, View::Tree(tree) if tree.directory.as_os_str().is_empty() && tree.entries.selection.selected().is_none())
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
        matches!(first.query(), Query::QueuePage { query: Some(query), offset: 0, .. } if query == "duplicate")
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
        matches!(lookup.query(), Query::QueueEntries { query: Some(query), queue_ids } if query == "duplicate" && queue_ids == vec![202])
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
    assert_eq!(page.offset, 0);
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
        clear.query(),
        Query::QueuePage {
            query: None,
            offset: 0,
            ..
        }
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
        matches!(first.query(), Query::PlaylistEntries { playlist_id: 1, query: Some(query), .. } if query == "song")
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
        matches!(lookup.query(), Query::PlaylistEntryLocations { playlist_id: 1, query: Some(query), entry_ids } if query == "song" && entry_ids == vec![1])
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
        tui.ui.query_request(&next).unwrap().query(),
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
        matches!(request.query(), Query::DirectoryPage { path, query: Some(query), .. } if path == Path::new("/music") && query == "child")
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
        matches!(ui.query_request(&state).unwrap().query(), Query::DirectoryPage { path, query: None, offset: 0, .. } if path == Path::new("/music/child"))
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
        matches!(locate.query(), Query::LibraryLocate { track_id: 42, path, tree: true, query: Some(query), .. } if path == Path::new("/music") && query == "song")
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
        matches!(request.query(), Query::OtherTracksPage { query: Some(query), favorite: Some(true), missing: Some(false), sort: LibrarySort::Album, .. } if query == "outside")
    );
    key(&mut ui, &state, KeyCode::Esc);
    let cleared = ui.query_request(&state).unwrap();
    assert!(
        matches!(cleared.query(), Query::OtherTracksPage { query: Some(query), favorite: Some(true), missing: Some(false), sort: LibrarySort::Album, .. } if query.is_empty())
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
    recent.sync(&state.library.history, "");
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
    key(&mut tui.ui, &next, KeyCode::Esc);
    let View::Recent(recent) = &tui.ui.view else {
        panic!("recent");
    };
    assert_eq!(recent.entries.total, 3);
    assert_eq!(recent.entries.selection.selected(), Some(0));
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
        matches!(locate.query(), Query::LibraryLocate { track_id: 1, query: Some(query), tree: false, .. } if query == "active")
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
    assert_eq!(tui.ui.query_request(&next).unwrap().offset, 6);
    // Another revision before the relocated page arrives must retain the ID,
    // rather than treating its new offset as an identity.
    next.library.revision += 1;
    tui.apply_snapshot(next.clone());
    assert!(matches!(
        tui.ui.query_request(&next).unwrap().query(),
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
        matches!(request.query(), Query::PlaylistSummaries { query: Some(query), offset: 0, .. } if query == "collection")
    );
    assert!(ui.apply_query(&state, &request, Ok(response(&request, 2))));
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
            stale.query(),
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
        assert_eq!(lookup.kind, stale.kind);
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
        assert_eq!(request.kind, QueryKind::Playlist(1));
        assert_eq!(request.offset, 0);
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
