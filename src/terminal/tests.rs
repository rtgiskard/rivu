use super::render::tests::render_view;
use super::*;
use crate::{model::Page, response::QueryRevisions};
use ratatui::{backend::TestBackend, text::Line};

#[test]
fn startup_hint_expires_once_at_its_deadline() {
    let mut ui = UiState::default();
    let deadline = ui.startup_hint_deadline.unwrap();
    assert!(!ui.expire_startup_hint(deadline - Duration::from_nanos(1)));
    assert!(ui.startup_hint_deadline.is_some());
    assert!(ui.expire_startup_hint(deadline));
    assert!(ui.startup_hint_deadline.is_none());
    assert!(!ui.expire_startup_hint(deadline));
}

#[test]
fn every_notice_permanently_dismisses_startup_hint() {
    for kind in [
        NoticeKind::Information,
        NoticeKind::Warning,
        NoticeKind::Error,
    ] {
        let mut ui = UiState::default();
        ui.set_notice("Operation result".into(), kind);
        assert!(ui.startup_hint_deadline.is_none());
        ui.message.as_mut().unwrap().expires_at = Instant::now();
        assert!(ui.expire_message());
        assert!(ui.message.is_none());
        assert!(ui.startup_hint_deadline.is_none());
    }
}

#[test]
fn closing_help_or_returning_to_queue_never_restores_startup_hint() {
    let state = TuiSnapshot::default();
    for key in [KeyCode::F(1), KeyCode::Char('?'), KeyCode::Tab] {
        let mut ui = UiState::default();
        ui.key_action(KeyEvent::from(key), &state);
        assert!(ui.startup_hint_deadline.is_none());
        ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
        assert!(!ui.show_help);
        assert!(matches!(ui.view, View::Queue));
        assert!(ui.startup_hint_deadline.is_none());
    }
    let mut ui = UiState::default();
    let action = ui.key_action(KeyEvent::from(KeyCode::Char('/')), &state);
    assert!(matches!(action, KeyAction::Search));
    assert!(ui.startup_hint_deadline.is_none());
}

pub(super) fn terminal_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in buffer.area.y..buffer.area.bottom() {
        let mut x = buffer.area.x;
        while x < buffer.area.right() {
            let symbol = buffer[(x, y)].symbol();
            text.push_str(symbol);
            x += Line::from(symbol).width().max(1) as u16;
        }
        text.push('\n');
    }
    text
}

#[test]
fn details_capture_full_metadata_and_own_keys_without_mutation() {
    let state = TuiSnapshot::default();
    let mut ui = UiState::default();
    let mut row = track(0);
    row.title = "Long full title 夜航 ".repeat(15);
    row.album = "End-of-details album".into();
    ui.queue.set_viewport(2);
    ui.queue.insert(
        0,
        2,
        1,
        vec![QueueRow {
            id: 17,
            track_id: row.id,
            position: 0,
            offset: 0,
            track: row.clone(),
        }],
    );
    ui.key_action(KeyEvent::from(KeyCode::Char('i')), &state);
    assert!(ui.details.as_ref().unwrap().text.contains(&row.title));
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
    for key in [
        KeyCode::Delete,
        KeyCode::Enter,
        KeyCode::Char(' '),
        KeyCode::Char('/'),
        KeyCode::F(1),
        KeyCode::Tab,
        KeyCode::BackTab,
    ] {
        assert!(matches!(
            ui.key_action(KeyEvent::from(key), &state),
            KeyAction::Ignored
        ));
        assert!(ui.details.is_some());
    }
    ui.key_action(KeyEvent::from(KeyCode::End), &state);
    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
    assert!(terminal_text(&terminal).contains(&row.album));
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    assert!(ui.details.is_none());
    assert_eq!(ui.queue.selected().unwrap().id, 17);
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
        KeyAction::Command(Command::RemoveQueue { queue_id: 17 })
    ));
}

#[test]
fn notices_preserve_actionable_failures_across_playback_updates() {
    let mut ui = UiState::default();
    ui.set_notice("failure".into(), NoticeKind::Error);
    ui.set_notice("playing".into(), NoticeKind::Information);
    assert!(matches!(
        ui.message.as_ref().unwrap().kind,
        NoticeKind::Error
    ));
    ui.message.as_mut().unwrap().expires_at = Instant::now();
    ui.set_notice("playing".into(), NoticeKind::Information);
    assert!(matches!(
        ui.message.as_ref().unwrap().kind,
        NoticeKind::Information
    ));
}

#[test]
fn queue_pages_keep_global_navigation_and_duplicate_occurrence_actions() {
    let mut state = TuiSnapshot::default();
    state.queue.total = 200_000;
    state.system.page_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.page_size = 4;
    tui.ui.queue.selection.select(Some(50_000));
    *tui.ui.queue.selection.offset_mut() = 49_999;
    tui.ui.queue.set_viewport(4);
    load_queue(&mut tui.ui, &state);
    assert_eq!(tui.ui.queue.selection.selected(), Some(50_000));
    assert_eq!(tui.ui.queue.selection.offset(), 49_999);
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
        KeyAction::Command(Command::PlayQueue { queue_id: 51_000 })
    ));
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
        KeyAction::Command(Command::RemoveQueue { queue_id: 51_000 })
    ));
    state.queue.current_id = Some(51_000);
    state.playback.position = 42.;
    tui.apply_snapshot(state.clone());
    assert!(tui.ui.query_request(&state).is_none());
    tui.ui.key_action(KeyEvent::from(KeyCode::PageDown), &state);
    assert_eq!(tui.ui.queue.selection.selected(), Some(50_004));
    tui.ui.key_action(KeyEvent::from(KeyCode::PageUp), &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Up), &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    assert_eq!(tui.ui.queue.selection.selected(), Some(50_000));
    tui.ui.queue.selection.select(Some(199_999));
    tui.ui.queue.set_viewport(4);
    load_queue(&mut tui.ui, &state);
    assert_eq!(tui.ui.queue.selected().unwrap().position, 199_999);
    assert!(tui.ui.queue.pages.len() <= 4usize.div_ceil(3) + 3);
    assert!(tui.ui.queue.pages.values().map(Vec::len).sum::<usize>() <= 15);
}

#[test]
fn queue_rejects_delayed_pages_and_selection_lookups_after_navigation() {
    let mut state = TuiSnapshot::default();
    state.queue.total = 30;
    state.system.page_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    let delayed = tui.ui.query_request(&state).unwrap();
    assert!(matches!(
        delayed.query(),
        Query::QueuePage {
            query: None,
            offset: 0,
            limit: 3
        }
    ));
    tui.ui.page_size = 8;
    tui.ui.key_action(KeyEvent::from(KeyCode::PageDown), &state);
    assert!(
        !tui.ui
            .apply_query(&state, &delayed, Ok(response(&delayed, 30)))
    );
    load_queue(&mut tui.ui, &state);
    state.queue.revision = 1;
    tui.apply_snapshot(state.clone());
    let relocation = tui.ui.query_request(&state).unwrap();
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    assert!(!tui.ui.apply_query(
        &state,
        &relocation,
        Ok(QueryResult {
            revisions: QueryRevisions {
                queue: 1,
                ..QueryRevisions::default()
            },
            view: ViewResponse::QueueEntries(vec![queue_row(0, 1008)]),
        })
    ));
    assert_eq!(tui.ui.queue.selection.selected(), Some(9));
    let request = tui.ui.query_request(&state).unwrap();
    let mut stale = response(&request, 30);
    stale.revisions.queue = 0;
    tui.ui.requested = Some(request.clone());
    assert!(!tui.ui.apply_query(&state, &request, Ok(stale)));
    assert!(tui.ui.queue.pages.is_empty());
    assert!(tui.ui.requested.is_none());
    assert!(
        tui.ui
            .apply_query(&state, &request, Err("queue unavailable".into()))
    );
    assert!(tui.ui.query_request(&state).is_none());
    for key in [
        KeyEvent::from(KeyCode::F(5)),
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
    ] {
        tui.ui.key_action(key, &state);
        assert!(tui.ui.query_request(&state).is_none());
    }
    state.queue.revision += 1;
    tui.apply_snapshot(state.clone());
    let refreshed = tui.ui.query_request(&state).unwrap();
    assert_ne!(refreshed.stamp, request.stamp);
    assert_ne!(refreshed.generation, request.generation);
    load_queue(&mut tui.ui, &state);
}

pub(super) fn queue_row(position: usize, id: u64) -> QueueRow {
    QueueRow {
        id,
        track_id: 7,
        position,
        offset: position,
        track: track(7),
    }
}

pub(super) fn load_queue(ui: &mut UiState, state: &TuiSnapshot) {
    for _ in 0..16 {
        let Some(request) = ui.query_request(state) else {
            return;
        };
        assert!(ui.apply_query(state, &request, Ok(response(&request, state.queue.total))));
    }
    panic!("queue viewport requests did not converge");
}

#[test]
fn empty_queue_changes_wait_for_occurrence_rows_before_actions() {
    let mut state = TuiSnapshot::default();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    assert!(tui.ui.query_request(&state).is_none());
    state.queue.total = 2;
    state.queue.revision = 1;
    tui.apply_snapshot(state.clone());
    assert_eq!(tui.ui.queue.selection.selected(), Some(0));
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
        KeyAction::Ignored
    ));
    load_queue(&mut tui.ui, &state);
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
        KeyAction::Command(Command::RemoveQueue { queue_id: 1000 })
    ));
    state.queue.total = 0;
    state.queue.revision = 2;
    tui.apply_snapshot(state);
    assert!(tui.ui.queue.pages.is_empty());
}

pub(super) fn track(index: usize) -> LibraryRow {
    LibraryRow {
        id: index as i64,
        title: format!("row-{index}"),
        artist: "artist".into(),
        album: "album".into(),
        duration: None,
        codec: "flac".into(),
        bitrate_bps: None,
        bitrate_exact: false,
        favorite: false,
        missing: false,
        play_count: 0,
    }
}

pub(super) fn response(request: &QueryRequest, total: usize) -> QueryResult {
    let indices = request.offset..(request.offset + request.limit).min(total);
    let view = match &request.kind {
        QueryKind::Queue => ViewResponse::QueuePage(Page {
            total,
            rows: indices
                .map(|position| queue_row(position, position as u64 + 1000))
                .collect(),
        }),
        QueryKind::QueueSelection(_) => {
            panic!("selection lookups require an explicit position")
        }
        QueryKind::Search(_) => ViewResponse::LibraryPage(Page {
            total,
            rows: indices.map(track).collect(),
        }),
        QueryKind::Directory(_) => ViewResponse::DirectoryPage(Page {
            total,
            rows: indices
                .map(|index| DirectoryRow::Track(track(index)))
                .collect(),
        }),
        QueryKind::Playlists => ViewResponse::PlaylistSummaries(Page {
            total,
            rows: indices
                .map(|index| PlaylistSummary {
                    id: index as i64,
                    name: format!("row-{index}"),
                    entry_count: 12,
                })
                .collect(),
        }),
        QueryKind::Playlist(_) => ViewResponse::PlaylistEntries(Page {
            total,
            rows: indices
                .map(|index| PlaylistEntryRow {
                    position: index,
                    offset: index,
                    id: index as i64,
                    track_id: index as i64,
                    title: format!("row-{index}"),
                    artist: "artist".into(),
                    album: "album".into(),
                    duration: None,
                    codec: String::new(),
                    favorite: false,
                    missing: false,
                })
                .collect(),
        }),
        QueryKind::PlaylistSelection {
            playlist_id,
            entry_id,
        } => ViewResponse::PlaylistEntryLocations(vec![crate::model::PlaylistEntryLocation {
            id: *entry_id,
            playlist_id: *playlist_id,
            position: *entry_id as usize,
            offset: *entry_id as usize,
            track_id: *entry_id,
            total,
        }]),
    };
    QueryResult {
        revisions: request.stamp.revisions(),
        view,
    }
}

pub(super) fn load_visible(ui: &mut UiState, state: &TuiSnapshot) {
    ui.prepare_local_view(state);
    for _ in 0..16 {
        let Some(request) = ui.query_request(state) else {
            return;
        };
        assert!(ui.apply_query(state, &request, Ok(response(&request, 12))));
    }
    panic!("visible requests did not converge");
}

pub(super) fn playlist_view(playlist_id: i64) -> View {
    View::PlaylistDetail {
        playlists: (ListState::default(), 1),
        detail: PlaylistDetail {
            playlist_id,
            name: "collection".into(),
            entries: PagedList::default(),
            entry_anchor: None,
            selected_entry_id: None,
        },
    }
}

pub(super) fn query_snapshot() -> TuiSnapshot {
    let mut state = TuiSnapshot::default();
    state.system.page_size = 3;
    state.queue.total = 12;
    state.queue.revision = 5;
    state.library.revision = 6;
    state.library.structure_revision = 7;
    state.library.playlist_revision = 8;
    Arc::make_mut(&mut state.library.playlist_entry_revisions).insert(1, 9);
    state
}

#[test]
fn tabs_wrap_in_both_directions_and_ignore_retired_shortcuts() {
    let state = query_snapshot();
    for backward in [false, true] {
        for key in [
            KeyEvent::from(if backward {
                KeyCode::BackTab
            } else {
                KeyCode::Tab
            }),
            KeyEvent::new(
                KeyCode::Tab,
                if backward {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            ),
        ] {
            let mut ui = UiState::default();
            let expected = if backward {
                [TopTab::Library, TopTab::Playlists, TopTab::Queue]
            } else {
                [TopTab::Playlists, TopTab::Library, TopTab::Queue]
            };
            for tab in expected.into_iter().cycle().take(6) {
                assert!(matches!(ui.key_action(key, &state), KeyAction::Ignored));
                assert_eq!(ui.view.tab(), tab);
                assert!(ui.startup_hint_deadline.is_none());
                for retired in ['t', 'P'] {
                    ui.key_action(KeyEvent::from(KeyCode::Char(retired)), &state);
                    assert_eq!(ui.view.tab(), tab);
                }
            }
        }
    }
}

#[test]
fn tab_round_trip_moves_cached_pages_and_preserves_subpage_coordinates() {
    let mut state = query_snapshot();
    Arc::make_mut(&mut state.library.playlist_entry_revisions).insert(5, 0);
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.queue.set_viewport(3);
    load_queue(&mut tui.ui, &state);
    for _ in 0..7 {
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    }
    let queue_position = (
        tui.ui.queue.selection.selected(),
        tui.ui.queue.selection.offset(),
    );
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    let View::Playlists(playlists) = &mut tui.ui.view else {
        unreachable!()
    };
    playlists.set_viewport(3);
    load_visible(&mut tui.ui, &state);
    for _ in 0..5 {
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    }
    tui.ui.key_action(KeyEvent::from(KeyCode::Right), &state);
    let View::PlaylistDetail { detail, .. } = &mut tui.ui.view else {
        unreachable!()
    };
    detail.entries.set_viewport(3);
    load_visible(&mut tui.ui, &state);
    for _ in 0..7 {
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    }
    load_visible(&mut tui.ui, &state);
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        unreachable!()
    };
    let entry = detail.entries.selected().unwrap() as *const PlaylistEntryRow;
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    let View::Tree(tree) = &mut tui.ui.view else {
        unreachable!()
    };
    tree.directory = "/music/disc".into();
    tree.ancestors.push((
        "/music".into(),
        ListState::default().with_selected(Some(4)).with_offset(2),
        3,
    ));
    tree.entries.set_viewport(3);
    load_visible(&mut tui.ui, &state);
    for _ in 0..7 {
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    }
    load_visible(&mut tui.ui, &state);
    let View::Tree(tree) = &tui.ui.view else {
        unreachable!()
    };
    let directory_row = tree.entries.selected().unwrap() as *const DirectoryRow;
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    assert_eq!(
        (
            tui.ui.queue.selection.selected(),
            tui.ui.queue.selection.offset()
        ),
        queue_position
    );
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    let View::Tree(tree) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(tree.directory, Path::new("/music/disc"));
    assert_eq!(tree.ancestors.len(), 1);
    assert_eq!(
        (
            tree.entries.selection.selected(),
            tree.entries.selection.offset()
        ),
        (Some(7), 5)
    );
    assert_eq!(
        tree.entries.selected().unwrap() as *const DirectoryRow,
        directory_row
    );
    assert!(tui.ui.query_request(&state).is_none());
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(detail.playlist_id, 5);
    assert_eq!(
        (
            detail.entries.selection.selected(),
            detail.entries.selection.offset()
        ),
        (Some(7), 5)
    );
    assert_eq!(
        detail.entries.selected().unwrap() as *const PlaylistEntryRow,
        entry
    );
    assert!(tui.ui.query_request(&state).is_none());
    tui.ui.key_action(KeyEvent::from(KeyCode::Left), &state);
    let View::Playlists(playlists) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(
        (playlists.selection.selected(), playlists.selection.offset()),
        (Some(5), 3)
    );
}

#[test]
fn switching_tabs_rejects_old_completion_even_after_returning_to_same_page() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    let old = tui.ui.query_request(&state).unwrap();
    tui.ui.requested = Some(old.clone());
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    assert!(tui.ui.requested.is_none());
    assert!(!tui.ui.apply_query(&state, &old, Ok(response(&old, 12))));
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    let current = tui.ui.query_request(&state).unwrap();
    assert_eq!(current.kind, old.kind);
    assert_eq!(current.stamp, old.stamp);
    assert_ne!(current.generation, old.generation);
    assert!(!tui.ui.apply_query(&state, &old, Ok(response(&old, 12))));
    assert!(
        tui.ui
            .apply_query(&state, &current, Ok(response(&current, 12)))
    );
}

#[test]
fn hidden_pages_invalidate_without_cancelling_visible_queue_and_handle_deletion() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.view = View::Tree(LibraryTree {
        directory: "/music".into(),
        ..LibraryTree::default()
    });
    load_visible(&mut tui.ui, &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    tui.ui.view = playlist_view(1);
    load_visible(&mut tui.ui, &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    let queue_request = tui.ui.query_request(&state).unwrap();
    tui.ui.requested = Some(queue_request.clone());
    let mut next = state.clone();
    next.library.revision += 1;
    tui.apply_snapshot(next.clone());
    assert_eq!(tui.ui.requested.as_ref(), Some(&queue_request));
    assert_eq!(tui.ui.query_request(&next), Some(queue_request.clone()));
    let View::Tree(tree) = tui.ui.cached_library.as_ref().unwrap() else {
        unreachable!()
    };
    assert!(tree.entries.pages.is_empty());
    assert_eq!(tree.directory, Path::new("/music"));
    let View::PlaylistDetail { detail, .. } = tui.ui.cached_playlists.as_ref().unwrap() else {
        unreachable!()
    };
    assert!(detail.entries.pages.is_empty());
    assert_eq!(detail.selected_entry_id, Some(0));
    Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 10);
    tui.apply_snapshot(next.clone());
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &next);
    assert_eq!(
        tui.ui.query_request(&next).unwrap().kind,
        QueryKind::PlaylistSelection {
            playlist_id: 1,
            entry_id: 0
        }
    );
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &next);
    Arc::make_mut(&mut next.library.playlist_entry_revisions).remove(&1);
    next.library.playlist_revision += 1;
    tui.apply_snapshot(next.clone());
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &next);
    assert!(matches!(tui.ui.view, View::Playlists(_)));
    assert_eq!(
        tui.ui.query_request(&next).unwrap().kind,
        QueryKind::Playlists
    );
}

#[test]
fn hidden_pages_follow_page_size_and_library_root_changes() {
    let mut state = query_snapshot();
    state.system.library_roots = Arc::new(vec!["/one".into()]);
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    load_visible(&mut tui.ui, &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    assert!(tui.ui.prepare_local_view(&state));
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    let old = tui.ui.query_request(&state).unwrap();
    let mut next = state.clone();
    next.system.page_size = 5;
    next.system.library_roots = Arc::new(vec!["/two".into()]);
    tui.apply_snapshot(next.clone());
    let View::Playlists(playlists) = tui.ui.cached_playlists.as_ref().unwrap() else {
        unreachable!()
    };
    assert!(playlists.pages.is_empty());
    assert_eq!(playlists.selection.selected(), Some(0));
    let current = tui.ui.query_request(&next).unwrap();
    assert_eq!(current.limit, 5);
    assert_ne!(current.generation, old.generation);
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &next);
    assert!(tui.ui.prepare_local_view(&next));
    let View::Tree(tree) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(tree.roots, vec![PathBuf::from("/two")]);
    assert!(
        matches!(tree.entries.selected(), Some(DirectoryRow::Directory { path }) if path == Path::new("/two"))
    );
}

#[test]
fn search_retains_editor_traversal_and_cached_library_while_highlighting_library() {
    let state = query_snapshot();
    let mut ui = UiState::new(Arc::new(Localizer::new("en-US")));
    ui.view = View::Tree(LibraryTree {
        directory: "/music/disc".into(),
        ..LibraryTree::default()
    });
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('/')), &state),
        KeyAction::Search
    ));
    ui.open_search();
    for key in [KeyCode::Tab, KeyCode::BackTab] {
        ui.key_action(KeyEvent::from(key), &state);
        assert_eq!(ui.view.tab(), TopTab::Library);
    }
    assert!(matches!(&ui.view, View::Search(search) if search.focus == SearchFocus::Query));
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    assert!(matches!(ui.view, View::Queue));
    ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    assert!(matches!(&ui.view, View::Tree(tree) if tree.directory == Path::new("/music/disc")));
}

#[test]
fn unrelated_updates_preserve_pending_and_loaded_terminal_pages() {
    for view in [
        View::Queue,
        View::Search(LibrarySearch::default()),
        View::Tree(LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        }),
        View::Playlists(PagedList::default()),
        playlist_view(1),
    ] {
        let state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
        tui.ui.view = view;
        let request = tui.ui.query_request(&state).unwrap();
        tui.ui.requested = Some(request.clone());
        let mut next = state.clone();
        next.library.revision += 1;
        next.library.structure_revision += 1;
        next.library.playlist_revision += 1;
        next.queue.revision += 1;
        Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 10);
        Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(2, 11);
        match request.kind {
            QueryKind::Queue => next.queue.revision = state.queue.revision,
            QueryKind::Search(_) => next.library.revision = state.library.revision,
            QueryKind::Directory(_) => {
                next.library.revision = state.library.revision;
                next.library.structure_revision = state.library.structure_revision;
            }
            QueryKind::Playlists => {
                next.library.playlist_revision = state.library.playlist_revision
            }
            QueryKind::Playlist(_) => {
                next.library.revision = state.library.revision;
                Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 9);
            }
            _ => unreachable!(),
        }
        tui.apply_snapshot(next.clone());
        assert_eq!(tui.ui.requested.as_ref(), Some(&request));
        assert_eq!(tui.ui.query_request(&next), Some(request.clone()));
        let mut result = response(&request, 12);
        result.revisions = next.query_revisions(request.kind.playlist_id());
        assert!(tui.ui.apply_query(&next, &request, Ok(result)));
        load_visible(&mut tui.ui, &next);
        let generation = tui.ui.projection.generation();
        tui.apply_snapshot(state);
        assert_eq!(tui.ui.projection.generation(), generation);
        assert!(tui.ui.query_request(&tui.snapshot).is_none());
    }
}

#[test]
fn directory_pages_refresh_for_metadata_and_structure_but_not_publication() {
    for metadata in [true, false] {
        let state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
        tui.ui.view = View::Tree(LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        });
        let first = tui.ui.query_request(&state).unwrap();
        assert!(tui.ui.apply_query(&state, &first, Ok(response(&first, 12))));
        let pending = tui.ui.query_request(&state).unwrap();
        let mut next = state.clone();
        if metadata {
            next.library.revision += 1;
        } else {
            next.library.structure_revision += 1;
        }
        tui.apply_snapshot(next.clone());
        assert!(
            !tui.ui
                .apply_query(&next, &pending, Ok(response(&pending, 12)))
        );
        let View::Tree(tree) = &tui.ui.view else {
            unreachable!()
        };
        assert!(tree.entries.pages.is_empty());
        let fresh = tui.ui.query_request(&next).unwrap();
        assert_ne!(fresh.stamp, first.stamp);
        let mut result = response(&fresh, 12);
        if metadata {
            result.revisions.library = state.library.revision;
        } else {
            result.revisions.structure = state.library.structure_revision;
        }
        assert!(!tui.ui.apply_query(&next, &fresh, Ok(result)));
        load_visible(&mut tui.ui, &next);
        next.system.revision += 1;
        tui.apply_snapshot(next.clone());
        assert!(tui.ui.query_request(&next).is_none());
    }
}

#[test]
fn playlist_metadata_refresh_keeps_occurrence_and_compact_lookup_scope() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.view = playlist_view(1);
    load_visible(&mut tui.ui, &state);
    let mut next = state.clone();
    next.library.revision += 1;
    tui.apply_snapshot(next.clone());
    let rows = tui.ui.query_request(&next).unwrap();
    assert_eq!(rows.kind, QueryKind::Playlist(1));
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(detail.selected_entry_id, Some(0));
    assert!(detail.entry_anchor.is_none());
    // The metadata refresh emptied pages; a subsequent reorder must still know
    // the selected occurrence rather than treating cache eviction as deletion.
    Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 10);
    tui.apply_snapshot(next.clone());
    let lookup = tui.ui.query_request(&next).unwrap();
    assert_eq!(
        lookup.kind,
        QueryKind::PlaylistSelection {
            playlist_id: 1,
            entry_id: 0
        }
    );
    assert!(!tui.ui.apply_query(&next, &rows, Ok(response(&rows, 12))));
    tui.ui.requested = Some(lookup.clone());
    next.library.revision += 1;
    next.library.playlist_revision += 1;
    next.queue.revision += 1;
    tui.apply_snapshot(next.clone());
    assert_eq!(tui.ui.requested.as_ref(), Some(&lookup));
    assert_eq!(tui.ui.query_request(&next), Some(lookup.clone()));
    let mut located = response(&lookup, 12);
    located.revisions = next.query_revisions(Some(1));
    assert!(tui.ui.apply_query(&next, &lookup, Ok(located)));
    let rows = tui.ui.query_request(&next).unwrap();
    assert_eq!(rows.kind, QueryKind::Playlist(1));
    assert_eq!(
        rows.stamp,
        rows.query().stamp(next.query_revisions(Some(1)))
    );
    // An empty compact result is an observed removed occurrence, with the
    // scoped token still captured atomically by PlaylistEntryLocations.
    Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 11);
    tui.apply_snapshot(next.clone());
    let removed = tui.ui.query_request(&next).unwrap();
    assert!(tui.ui.apply_query(
        &next,
        &removed,
        Ok(QueryResult {
            revisions: next.query_revisions(Some(1)),
            view: ViewResponse::PlaylistEntryLocations(Vec::new()),
        })
    ));
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        unreachable!()
    };
    assert!(detail.entry_anchor.is_none());
    assert!(detail.selected_entry_id.is_none());
    assert_eq!(
        tui.ui.query_request(&next).unwrap().kind,
        QueryKind::Playlist(1)
    );
}

#[test]
fn deleted_playlist_returns_to_summaries_and_rejects_pending_rows() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.view = playlist_view(1);
    let request = tui.ui.query_request(&state).unwrap();
    let mut next = state.clone();
    Arc::make_mut(&mut next.library.playlist_entry_revisions).remove(&1);
    next.library.playlist_revision += 1;
    tui.apply_snapshot(next.clone());
    assert!(matches!(tui.ui.view, View::Playlists(_)));
    assert!(
        !tui.ui
            .apply_query(&next, &request, Ok(response(&request, 12)))
    );
    assert_eq!(
        tui.ui.query_request(&next).unwrap().kind,
        QueryKind::Playlists
    );
}

#[test]
fn terminal_page_demand_does_not_change_projection_but_page_size_does() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.view = View::Playlists(PagedList::default());
    let first = tui.ui.query_request(&state).unwrap();
    assert!(tui.ui.apply_query(&state, &first, Ok(response(&first, 12))));
    let second = tui.ui.query_request(&state).unwrap();
    assert_ne!(first.offset, second.offset);
    assert_eq!(first.generation, second.generation);
    assert_eq!(first.stamp, second.stamp);
    let mut next = state.clone();
    next.system.page_size = 5;
    tui.apply_snapshot(next.clone());
    assert!(
        !tui.ui
            .apply_query(&next, &second, Ok(response(&second, 12)))
    );
    let resized = tui.ui.query_request(&next).unwrap();
    assert_eq!(resized.limit, 5);
    assert_ne!(resized.generation, second.generation);
    assert_eq!(resized.stamp, second.stamp);
}

#[test]
fn failed_playlist_pages_ignore_retry_keys_and_recover_after_reopening() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.view = View::Playlists(PagedList::default());
    let request = tui.ui.query_request(&state).unwrap();
    assert!(
        tui.ui
            .apply_query(&state, &request, Err("unavailable".into()))
    );
    let mut next = state.clone();
    next.queue.revision += 1;
    tui.apply_snapshot(next.clone());
    assert!(tui.ui.query_request(&next).is_none());
    tui.ui.key_action(KeyEvent::from(KeyCode::Char('?')), &next);
    assert!(tui.ui.query_request(&next).is_none());
    tui.ui.key_action(KeyEvent::from(KeyCode::Esc), &next);
    for key in [
        KeyEvent::from(KeyCode::F(5)),
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
    ] {
        tui.ui.key_action(key, &next);
        assert!(tui.ui.query_request(&next).is_none());
    }
    tui.ui.key_action(KeyEvent::from(KeyCode::Esc), &next);
    assert!(matches!(tui.ui.view, View::Queue));
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &next);
    assert!(matches!(tui.ui.view, View::Playlists(_)));
    let reopened = tui.ui.query_request(&next).unwrap();
    assert_eq!(reopened.stamp, request.stamp);
    assert_ne!(reopened.generation, request.generation);
    assert!(
        !tui.ui
            .apply_query(&next, &request, Ok(response(&request, 12)))
    );
    assert!(
        tui.ui
            .apply_query(&next, &reopened, Ok(response(&reopened, 12)))
    );
}

#[test]
fn playlist_reorder_preserves_unrelated_terminal_views() {
    let mut snapshot = TuiSnapshot::default();
    snapshot.system.page_size = 3;
    Arc::make_mut(&mut snapshot.library.playlist_entry_revisions).insert(2, 0);
    for view in [
        View::Search(LibrarySearch::default()),
        View::Tree(LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        }),
        View::Playlists(PagedList::default()),
        View::PlaylistDetail {
            playlists: (ListState::default(), 1),
            detail: PlaylistDetail {
                playlist_id: 2,
                name: "other".into(),
                entries: PagedList::default(),
                entry_anchor: None,
                selected_entry_id: None,
            },
        },
    ] {
        let mut tui = TuiState::new(snapshot.clone(), Arc::new(Localizer::new("en-US")));
        tui.ui.view = view;
        load_visible(&mut tui.ui, &snapshot);
        let generation = tui.ui.projection.generation();
        let mut next = snapshot.clone();
        Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 1);
        tui.apply_snapshot(next);
        assert_eq!(tui.ui.projection.generation(), generation);
        assert!(tui.ui.query_request(&tui.snapshot).is_none());
    }
}

#[test]
fn terminal_playlist_reorder_relocates_identity_and_rejects_stale_results() {
    let mut snapshot = TuiSnapshot::default();
    snapshot.system.page_size = 1;
    Arc::make_mut(&mut snapshot.library.playlist_entry_revisions).insert(1, 0);
    let mut tui = TuiState::new(snapshot, Arc::new(Localizer::new("en-US")));
    tui.ui.view = View::PlaylistDetail {
        playlists: (ListState::default(), 1),
        detail: PlaylistDetail {
            playlist_id: 1,
            name: "one".into(),
            entries: PagedList::default(),
            entry_anchor: None,
            selected_entry_id: None,
        },
    };
    let first = tui.ui.query_request(&tui.snapshot).unwrap();
    assert!(
        tui.ui
            .apply_query(&tui.snapshot, &first, Ok(response(&first, 12)))
    );
    let pending = tui.ui.query_request(&tui.snapshot).unwrap();
    let old_page = response(&pending, 12);
    let mut next = tui.snapshot.clone();
    Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 1);
    tui.apply_snapshot(next);
    assert!(!tui.ui.apply_query(&tui.snapshot, &pending, Ok(old_page)));
    let lookup = tui.ui.query_request(&tui.snapshot).unwrap();
    assert_eq!(
        lookup.kind,
        QueryKind::PlaylistSelection {
            playlist_id: 1,
            entry_id: 0
        }
    );
    let old_lookup = response(&lookup, 12);
    let mut next = tui.snapshot.clone();
    Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 2);
    tui.apply_snapshot(next);
    assert!(!tui.ui.apply_query(&tui.snapshot, &lookup, Ok(old_lookup)));
    let lookup = tui.ui.query_request(&tui.snapshot).unwrap();
    let mut located = response(&lookup, 12);
    let ViewResponse::PlaylistEntryLocations(entries) = &mut located.view else {
        panic!("expected entry locations");
    };
    entries[0].position = 9;
    assert!(tui.ui.apply_query(&tui.snapshot, &lookup, Ok(located)));
    let page = tui.ui.query_request(&tui.snapshot).unwrap();
    assert_eq!(page.kind, QueryKind::Playlist(1));
    assert_eq!(page.offset, 9);
    let mut relocated = response(&page, 12);
    let ViewResponse::PlaylistEntries(entries) = &mut relocated.view else {
        panic!("expected entries");
    };
    entries.rows[0].id = 0;
    entries.rows[0].track_id = 0;
    assert!(tui.ui.apply_query(&tui.snapshot, &page, Ok(relocated)));
    assert!(matches!(
        tui.ui
            .key_action(KeyEvent::from(KeyCode::Enter), &tui.snapshot),
        KeyAction::Command(Command::Play { track_id: 0 })
    ));
}

#[test]
fn every_queried_view_renders_across_batches_without_resetting_scroll() {
    let mut state = TuiSnapshot::default();
    state.system.page_size = 3;
    Arc::make_mut(&mut state.library.playlist_entry_revisions).insert(7, 0);
    let views = [
        View::Search(LibrarySearch {
            focus: SearchFocus::Results,
            ..LibrarySearch::default()
        }),
        View::Tree(LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        }),
        View::Playlists(PagedList::default()),
        View::PlaylistDetail {
            playlists: (ListState::default(), 1),
            detail: PlaylistDetail {
                playlist_id: 7,
                name: "collection".into(),
                entries: PagedList::default(),
                entry_anchor: None,
                selected_entry_id: None,
            },
        },
    ];
    for view in views {
        let mut ui = UiState {
            view,
            page_size: 4,
            ..UiState::default()
        };
        render_view(&mut ui);
        load_visible(&mut ui, &state);
        for _ in 0..3 {
            ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        }
        let rendered = render_view(&mut ui);
        assert!(rendered.contains("row-0"), "{rendered}");
        assert!(rendered.contains("row-3"), "{rendered}");
        let selected = ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
        match selected {
            KeyAction::Command(Command::PlayPlaylist { playlist_id: 3 })
            | KeyAction::Command(Command::Play { track_id: 3 }) => {}
            _ => panic!("selection did not activate the global row"),
        }
        ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        load_visible(&mut ui, &state);
        let rendered = render_view(&mut ui);
        assert!(!rendered.contains("row-0"), "{rendered}");
        assert!(rendered.contains("row-1"), "{rendered}");
        assert!(rendered.contains("row-4"), "{rendered}");
    }
}

#[test]
fn viewport_navigation_and_retention_do_not_depend_on_batch_size() {
    for fetch_size in [1, 3, 32] {
        let mut list = PagedList::default();
        list.set_viewport(10);
        for down in [true, false] {
            for _ in 0..200 {
                let mut requests = 0;
                while let Some(offset) = list.next_offset(fetch_size) {
                    let rows = (offset..(offset + fetch_size).min(10_000)).collect();
                    list.insert(offset, fetch_size, 10_000, rows);
                    requests += 1;
                    assert!(requests <= 10usize.div_ceil(fetch_size) + 4);
                }
                assert_eq!(list.total, 10_000);
                assert!(list.pages.len() <= 10usize.div_ceil(fetch_size) + 3);
                assert!(
                    (list.selection.offset()..list.visible_end())
                        .all(|index| list.get(index) == Some(&index))
                );
                let before = list.selection.selected().unwrap();
                list.navigate_action(
                    if down {
                        Action::PageDown
                    } else {
                        Action::PageUp
                    },
                    10,
                );
                assert_eq!(
                    list.selection.selected(),
                    Some(if down {
                        before + 10
                    } else {
                        before.saturating_sub(10)
                    })
                );
            }
        }
    }
}

#[test]
fn tree_top_level_contains_only_configured_roots_and_restores_parent() {
    let mut state = TuiSnapshot::default();
    state.system.page_size = 1;
    for roots in [
        vec![],
        vec![PathBuf::from("/music")],
        vec![
            "/first".into(),
            "/second".into(),
            "/third".into(),
            "/fourth".into(),
            "/fifth".into(),
        ],
        vec!["/".into()],
    ] {
        let mut ui = UiState {
            view: View::Tree(LibraryTree::new(&roots)),
            page_size: 4,
            ..UiState::default()
        };
        render_view(&mut ui);
        load_visible(&mut ui, &state);
        assert!(ui.query_request(&state).is_none());
        let View::Tree(tree) = &ui.view else {
            unreachable!()
        };
        assert_eq!(tree.entries.total, roots.len());
        assert!(tree.entries.loaded);
        if roots.is_empty() {
            assert!(tree.entries.selection.selected().is_none());
            continue;
        }
        let mut expected = roots.clone();
        expected.sort();
        for path in expected {
            load_visible(&mut ui, &state);
            let View::Tree(tree) = &ui.view else {
                unreachable!()
            };
            assert_eq!(
                tree.entries.selected(),
                Some(&DirectoryRow::Directory { path: path.clone() })
            );
            ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
            let request = ui.query_request(&state).unwrap();
            assert_eq!(request.kind, QueryKind::Directory(path));
            ui.key_action(KeyEvent::from(KeyCode::Left), &state);
            load_visible(&mut ui, &state);
            ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        }
    }
}

#[test]
fn stale_revisions_do_not_stall_navigation() {
    let mut state = TuiSnapshot::default();
    state.system.page_size = 3;
    let mut ui = UiState {
        view: View::Playlists(PagedList::default()),
        ..UiState::default()
    };
    let request = ui.query_request(&state).unwrap();
    let mut stale = response(&request, 12);
    stale.revisions.playlist = 1;
    ui.requested = Some(request.clone());
    assert!(!ui.apply_query(&state, &request, Ok(stale)));
    assert!(ui.requested.is_none());
    load_visible(&mut ui, &state);
}

#[test]
fn returning_to_parent_restores_selection_and_middle_of_viewport() {
    let state = TuiSnapshot::default();
    let mut tree = LibraryTree::default();
    tree.entries.selection = ListState::default().with_selected(Some(34)).with_offset(30);
    tree.entries.set_viewport(10);
    tree.entries.insert(
        0,
        100,
        100,
        (0..100)
            .map(|index| DirectoryRow::Directory {
                path: format!("/music/{index}").into(),
            })
            .collect(),
    );
    let mut ui = UiState {
        view: View::Tree(tree),
        ..UiState::default()
    };
    ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    let View::Tree(tree) = &mut ui.view else {
        panic!("expected parent directory")
    };
    tree.entries.insert(
        20,
        20,
        100,
        (20..40)
            .map(|index| DirectoryRow::Track(track(index)))
            .collect(),
    );
    tree.entries.set_viewport(10);
    assert_eq!(tree.entries.selection.selected(), Some(34));
    assert_eq!(tree.entries.selection.offset(), 30);

    let mut playlists = PagedList {
        selection: ListState::default().with_selected(Some(34)).with_offset(30),
        ..PagedList::default()
    };
    playlists.set_viewport(10);
    playlists.insert(
        0,
        100,
        100,
        (0..100)
            .map(|index| PlaylistSummary {
                id: index,
                name: format!("collection-{index}"),
                entry_count: 1,
            })
            .collect(),
    );
    ui.view = View::Playlists(playlists);
    ui.key_action(KeyEvent::from(KeyCode::Right), &state);
    ui.key_action(KeyEvent::from(KeyCode::Left), &state);
    let View::Playlists(playlists) = &mut ui.view else {
        panic!("expected playlist catalog")
    };
    playlists.insert(
        20,
        20,
        100,
        (20..40)
            .map(|index| PlaylistSummary {
                id: index,
                name: format!("collection-{index}"),
                entry_count: 1,
            })
            .collect(),
    );
    playlists.set_viewport(10);
    assert_eq!(playlists.selection.selected(), Some(34));
    assert_eq!(playlists.selection.offset(), 30);
}

#[test]
fn interrupt_exits_editors_and_overlays_without_dispatching_core_commands() {
    let state = TuiSnapshot::default();
    let mut screens = vec![
        UiState::default(),
        UiState {
            view: View::Search(LibrarySearch::default()),
            ..UiState::default()
        },
        UiState {
            show_help: true,
            ..UiState::default()
        },
        UiState {
            details: Some(Details {
                text: "metadata".into(),
                wrapped: Vec::new(),
                width: 0,
                scroll: 0,
                max_scroll: 0,
            }),
            ..UiState::default()
        },
    ];
    let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    // The loading screen resolves the same shared binding before UiState exists.
    assert_eq!(
        resolve_key(interrupt, KeyboardContext::TuiQueue),
        Some(Action::Close)
    );
    for ui in &mut screens {
        assert!(matches!(ui.key_action(interrupt, &state), KeyAction::Quit));
    }
}

#[test]
fn question_mark_opens_help_except_when_owned_by_query_input() {
    let state = TuiSnapshot::default();
    for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
        let mut ui = UiState::default();
        let help = KeyEvent::new(KeyCode::Char('?'), modifiers);
        ui.key_action(help, &state);
        assert!(ui.show_help);
        ui.key_action(help, &state);
        assert!(!ui.show_help);
        ui.view = View::Search(LibrarySearch::default());
        ui.key_action(help, &state);
        assert!(!ui.show_help);
        assert!(matches!(&ui.view, View::Search(search) if search.query == "?"));
        ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
        ui.key_action(help, &state);
        assert!(ui.show_help);
    }
}

#[test]
fn query_editor_keeps_printable_shortcuts_and_rejects_modified_text() {
    let state = TuiSnapshot::default();
    let mut ui = UiState {
        view: View::Search(LibrarySearch::default()),
        ..UiState::default()
    };
    for character in "R? q Aa/[]!春".chars() {
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char(character)), &state),
            KeyAction::Ignored
        ));
    }
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SUPER,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ] {
        ui.key_action(KeyEvent::new(KeyCode::Char('q'), modifiers), &state);
        ui.key_action(KeyEvent::new(KeyCode::Backspace, modifiers), &state);
    }
    let View::Search(search) = &ui.view else {
        panic!("query stays open")
    };
    assert_eq!(search.query, "R? q Aa/[]!春");
    ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    assert!(matches!(&ui.view, View::Search(search) if search.focus == SearchFocus::Results));
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('q')), &state),
        KeyAction::Ignored
    ));
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char(' ')), &state),
        KeyAction::Command(Command::Toggle)
    ));
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    assert!(matches!(&ui.view, View::Search(search) if search.focus == SearchFocus::Query));
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    assert!(matches!(ui.view, View::Queue));
}

#[test]
fn help_scrolls_every_binding_without_touching_the_underlying_queue() {
    for language in ["en-US", "zh-CN"] {
        let state = TuiSnapshot::default();
        let mut ui = UiState::new(Arc::new(Localizer::new(language)));
        ui.queue.total = 100;
        ui.queue.loaded = true;
        ui.queue.selection.select(Some(7));
        ui.key_action(KeyEvent::from(KeyCode::F(1)), &state);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let mut visible = String::new();
        for _ in 0..help_bindings(KeyboardContext::TuiQueue).count() {
            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
            visible.push_str(&terminal_text(&terminal));
            ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        }
        for binding in help_bindings(KeyboardContext::TuiQueue) {
            assert!(
                visible.contains(ui.i18n.text(binding.description)),
                "{}",
                binding.description
            );
        }
        for key in [
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('q'),
            KeyCode::Enter,
            KeyCode::Delete,
            KeyCode::Tab,
            KeyCode::BackTab,
        ] {
            assert!(matches!(
                ui.key_action(KeyEvent::from(key), &state),
                KeyAction::Ignored
            ));
        }
        assert!(matches!(ui.view, View::Queue));
        assert_eq!(ui.queue.selection.selected(), Some(7));
        assert!(ui.show_help);
        ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
        assert!(!ui.show_help);
        assert_eq!(ui.queue.selection.selected(), Some(7));
    }
}

#[test]
fn boundary_navigation_queries_unloaded_pages_and_rejects_old_demands() {
    let mut state = TuiSnapshot::default();
    state.queue.total = 100;
    state.system.page_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    let old = tui.ui.query_request(&state).unwrap();
    tui.ui.key_action(KeyEvent::from(KeyCode::End), &state);
    let last = tui.ui.query_request(&state).unwrap();
    assert_eq!(last.offset, 99);
    assert!(!tui.ui.apply_query(&state, &old, Ok(response(&old, 100))));
    tui.ui.key_action(KeyEvent::from(KeyCode::Home), &state);
    let first = tui.ui.query_request(&state).unwrap();
    assert_eq!(first.offset, 0);
    assert_ne!(first.generation, old.generation);
    assert!(!tui.ui.apply_query(&state, &last, Ok(response(&last, 100))));
    assert!(!tui.ui.apply_query(&state, &old, Ok(response(&old, 100))));
    assert!(
        tui.ui
            .apply_query(&state, &first, Ok(response(&first, 100)))
    );
}

#[test]
fn terminal_routes_playback_chords_and_exit_without_stopping_core() {
    let state = TuiSnapshot {
        playback: crate::model::PlaybackState {
            status: PlaybackStatus::Playing,
            position: 12.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut ui = UiState::default();
    assert!(
        matches!(ui.key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL), &state), KeyAction::Command(Command::Seek { seconds }) if seconds == 7.0)
    );
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char(' ')), &state),
        KeyAction::Command(Command::Toggle)
    ));
    for key in [
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Char('d'),
        KeyCode::Char('R'),
    ] {
        assert!(matches!(
            ui.key_action(KeyEvent::from(key), &state),
            KeyAction::Ignored
        ));
    }
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SHIFT,
        KeyModifiers::SUPER,
    ] {
        assert_eq!(
            resolve_key(
                KeyEvent::new(KeyCode::Char('q'), modifiers),
                KeyboardContext::TuiQueue
            ),
            None
        );
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Char('q'), modifiers), &state),
            KeyAction::Ignored
        ));
    }
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('q')), &state),
        KeyAction::Quit
    ));
}

#[test]
fn playlist_removal_targets_the_selected_entry_only() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: playlist_view(1),
        ..UiState::default()
    };
    load_visible(&mut ui, &state);
    ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
        KeyAction::Command(Command::RemovePlaylistEntry { entry_id: 1 })
    ));
    assert!(matches!(
        ui.key_action(
            KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL),
            &state
        ),
        KeyAction::Ignored
    ));
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('d')), &state),
        KeyAction::Ignored
    ));
}
