use super::*;
use crate::terminal::render::{draw, help_bindings, tests::render_view};
use ratatui::{Terminal, backend::TestBackend, text::Line};
use rivu_protocol::{
    model::{Page, PlaybackStatus},
    response::QueryRevisions,
};
use std::path::Path;

fn type_command(ui: &mut UiState, state: &TuiSnapshot, command: &str) -> KeyAction {
    ui.key_action(KeyEvent::from(KeyCode::Char(':')), state);
    for character in command.chars() {
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char(character)), state),
            KeyAction::Ignored
        ));
    }
    ui.key_action(KeyEvent::from(KeyCode::Enter), state)
}

#[test]
fn colon_commands_distinguish_local_exit_shutdown_and_help() {
    let state = TuiSnapshot::default();
    for command in ["q", "quit", " q "] {
        let mut ui = UiState::default();
        assert!(matches!(
            type_command(&mut ui, &state, command),
            KeyAction::Quit
        ));
        assert!(ui.command_input.is_none());
    }
    for command in ["qa", "qall"] {
        let mut ui = UiState::default();
        assert!(matches!(
            type_command(&mut ui, &state, command),
            KeyAction::Shutdown
        ));
        assert!(ui.command_input.is_none());
    }
    let mut ui = UiState::default();
    assert!(matches!(
        type_command(&mut ui, &state, "help"),
        KeyAction::Ignored
    ));
    assert!(ui.show_help);
    assert_eq!(ui.help_scroll, 0);
}

#[test]
fn command_editor_cancels_bounds_unicode_and_rejects_unknown_commands() {
    let state = TuiSnapshot::default();
    let mut ui = UiState::default();
    for command in ["", "   ", "!quit", "qa extra", "unknown"] {
        assert!(matches!(
            type_command(&mut ui, &state, command),
            KeyAction::Ignored
        ));
        assert!(ui.command_input.is_none());
        if !command.trim().is_empty() {
            let notice = ui.message.as_ref().unwrap();
            assert!(notice.kind == NoticeKind::Error);
            assert!(notice.text.contains(command));
        }
    }
    ui.key_action(KeyEvent::from(KeyCode::Char(':')), &state);
    for _ in 0..200 {
        ui.key_action(KeyEvent::from(KeyCode::Char('界')), &state);
    }
    assert_eq!(ui.command_input.as_ref().unwrap().len(), 255);
    ui.key_action(KeyEvent::from(KeyCode::Backspace), &state);
    assert_eq!(ui.command_input.as_ref().unwrap().len(), 252);
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    assert!(ui.command_input.is_none());
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('q')), &state),
        KeyAction::Quit
    ));
}

#[test]
fn colon_respects_search_and_modal_input_ownership() {
    let state = TuiSnapshot::default();
    let mut ui = UiState::default();
    ui.open_search();
    ui.key_action(KeyEvent::from(KeyCode::Char(':')), &state);
    assert_eq!(ui.search.query, ":");
    assert!(ui.command_input.is_none());
    ui.search.editing = false;
    ui.key_action(KeyEvent::from(KeyCode::Char(':')), &state);
    assert!(ui.command_input.is_some());
    assert_eq!(ui.search.query, ":");
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    ui.search = SearchState::default();
    for details in [false, true] {
        ui.show_help = !details;
        ui.details_active = details;
        ui.key_action(KeyEvent::from(KeyCode::Char(':')), &state);
        assert!(ui.command_input.is_none());
    }
    ui.details_active = false;
    ui.key_action(KeyEvent::from(KeyCode::Char(':')), &state);
    assert!(ui.command_input.is_some());
    assert!(matches!(
        ui.key_action(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &state
        ),
        KeyAction::Quit
    ));
}

fn shortcut_tree(rows: Vec<DirectoryRow>, other_tracks: bool) -> View {
    let mut tree = LibraryTree {
        directory: if other_tracks {
            PathBuf::new()
        } else {
            "/music".into()
        },
        other_tracks,
        ..LibraryTree::default()
    };
    tree.entries.set_viewport(rows.len().max(1));
    tree.entries.insert(0, rows.len(), rows.len(), rows);
    View::Tree(tree)
}

#[test]
fn enqueue_track_uses_e_on_library_search_other_and_playlist_only() {
    let state = query_snapshot();
    let mut results = PagedList::default();
    results.set_viewport(2);
    results.insert(0, 2, 2, vec![track(41), track(42)]);
    results.selection.select(Some(1));
    let mut playlist = UiState {
        view: playlist_view(1),
        ..UiState::default()
    };
    load_visible(&mut playlist, &state);
    let View::PlaylistDetail { detail, .. } = &mut playlist.view else {
        unreachable!()
    };
    detail.entries.selection.select(Some(1));
    assert_eq!(
        detail.entries.selected().map(|entry| entry.track_id),
        Some(1)
    );
    for (view, expected) in [
        (View::LibraryResults(results), 42),
        (
            shortcut_tree(vec![DirectoryRow::Track(track(42))], false),
            42,
        ),
        (
            shortcut_tree(vec![DirectoryRow::Track(track(42))], true),
            42,
        ),
        (playlist.view, 1),
    ] {
        let mut ui = UiState {
            view,
            ..UiState::default()
        };
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
            KeyModifiers::SUPER,
        ] {
            assert!(matches!(
                ui.key_action(KeyEvent::new(KeyCode::Char('e'), modifiers), &state),
                KeyAction::Ignored
            ));
        }
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char('a')), &state),
            KeyAction::Ignored
        ));
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char('e')), &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![expected]
        ));
        ui.show_help = true;
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char('e')), &state),
            KeyAction::Ignored
        ));
        ui.show_help = false;
        ui.details_active = true;
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char('e')), &state),
            KeyAction::Ignored
        ));
        ui.details_active = false;
        ui.open_search();
        assert_eq!(ui.keyboard_context(), KeyboardContext::TuiSearchQuery);
        ui.key_action(KeyEvent::from(KeyCode::Char('e')), &state);
        assert_eq!(ui.search.query, "e");
        for key in ['e', 'f', 'm', 's', 'c'] {
            assert!(matches!(
                ui.key_action(KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT), &state),
                KeyAction::Ignored
            ));
        }
        assert_eq!(ui.search.query, "e");
    }
    for view in [
        View::Queue,
        View::Recent(RecentTracks::default()),
        View::Playlists(PagedList::default()),
    ] {
        let mut ui = UiState {
            view,
            ..UiState::default()
        };
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char('e')), &state),
            KeyAction::Ignored
        ));
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT), &state),
            KeyAction::Ignored
        ));
    }
}

#[test]
fn collection_enqueue_targets_selected_directory_or_entire_other_not_library() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: shortcut_tree(
            vec![DirectoryRow::Directory {
                path: "/music/selected".into(),
            }],
            false,
        ),
        ..UiState::default()
    };
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('e')), &state),
        KeyAction::Ignored
    ));
    assert!(matches!(
        ui.key_action(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT), &state),
        KeyAction::Command(Command::EnqueueSources { directories, track_ids, other_tracks: false })
            if directories == vec![PathBuf::from("/music/selected")] && track_ids.is_empty()
    ));
    ui.view = shortcut_tree(vec![DirectoryRow::Track(track(42))], false);
    assert!(matches!(
        ui.key_action(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT), &state),
        KeyAction::Ignored
    ));
    for view in [
        shortcut_tree(vec![DirectoryRow::OtherTracks], false),
        shortcut_tree(vec![DirectoryRow::Track(track(42))], true),
        shortcut_tree(Vec::new(), true),
    ] {
        ui.view = view;
        if let View::Tree(tree) = &mut ui.view {
            tree.favorite = Some(true);
            tree.missing = Some(true);
            tree.sort = rivu_protocol::model::LibrarySort::Album;
        }
        ui.search.active = true;
        ui.search.applied_query = "selected subset".into();
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT), &state),
            KeyAction::Command(Command::EnqueueSources { directories, track_ids, other_tracks: true })
                if directories.is_empty() && track_ids.is_empty()
        ));
    }
    ui.view = shortcut_tree(Vec::new(), false);
    assert!(matches!(
        ui.key_action(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT), &state),
        KeyAction::Ignored
    ));
}

#[test]
fn other_actions_and_help_keep_their_existing_collection_scope() {
    let state = query_snapshot();
    let mut ui = UiState::default();
    for (view, context, check) in [
        (
            shortcut_tree(
                vec![DirectoryRow::Directory {
                    path: "/music/selected".into(),
                }],
                false,
            ),
            KeyboardContext::TuiTree,
            false,
        ),
        (
            shortcut_tree(vec![DirectoryRow::OtherTracks], false),
            KeyboardContext::TuiOtherTarget,
            true,
        ),
        (
            shortcut_tree(vec![DirectoryRow::Track(track(42))], true),
            KeyboardContext::TuiOther,
            true,
        ),
    ] {
        ui.view = view;
        assert_eq!(ui.keyboard_context(), context);
        for modifiers in [KeyModifiers::ALT, KeyModifiers::ALT | KeyModifiers::SHIFT] {
            let action = ui.key_action(KeyEvent::new(KeyCode::Char('c'), modifiers), &state);
            if check {
                assert!(
                    matches!(action, KeyAction::Command(Command::CheckOtherTracks { force })
                    if force == modifiers.contains(KeyModifiers::SHIFT))
                );
            } else {
                assert!(matches!(action, KeyAction::Ignored));
            }
        }
        assert!(matches!(
            ui.key_action(
                KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::ALT | KeyModifiers::CONTROL
                ),
                &state
            ),
            KeyAction::Ignored
        ));
        for key in ['f', 'm', 's'] {
            ui.key_action(KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT), &state);
        }
        let View::Tree(tree) = &ui.view else {
            unreachable!()
        };
        assert_eq!(
            tree.favorite,
            (context == KeyboardContext::TuiOther).then_some(true)
        );
        assert_eq!(
            tree.missing,
            (context == KeyboardContext::TuiOther).then_some(true)
        );
        assert_eq!(
            tree.sort,
            if context == KeyboardContext::TuiOther {
                rivu_protocol::model::LibrarySort::Album
            } else {
                rivu_protocol::model::LibrarySort::Id
            }
        );
        let rows = help_bindings(context).collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .any(|row| row.action == Action::CheckOtherTracks),
            check
        );
        assert_eq!(
            rows.iter().any(|row| row.action == Action::FilterFavorites),
            context == KeyboardContext::TuiOther
        );
        assert!(
            rows.iter()
                .any(|row| row.action == Action::EnqueueCollection && row.label() == "Alt+e")
        );
    }
}

#[test]
fn cancel_scan_remains_global_including_search_editor_with_exact_modifiers() {
    let mut state = query_snapshot();
    state.system.scanning = true;
    state.system.scan_task_id = Some(17);
    for view in [
        View::Queue,
        View::Recent(RecentTracks::default()),
        View::Playlists(PagedList::default()),
        shortcut_tree(vec![DirectoryRow::Track(track(42))], false),
        shortcut_tree(vec![DirectoryRow::Track(track(42))], true),
    ] {
        let mut ui = UiState {
            view,
            ..UiState::default()
        };
        for editing in [false, true] {
            ui.search.editing = editing;
            assert!(matches!(
                ui.key_action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), &state),
                KeyAction::Command(Command::CancelScan { id: 17 })
            ));
            assert!(matches!(
                ui.key_action(
                    KeyEvent::new(
                        KeyCode::Char('x'),
                        KeyModifiers::ALT | KeyModifiers::CONTROL
                    ),
                    &state
                ),
                KeyAction::Ignored
            ));
        }
    }
    state.system.scanning = false;
    let mut ui = UiState::default();
    assert!(matches!(
        ui.key_action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), &state),
        KeyAction::Ignored
    ));
}

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

pub(in crate::terminal) fn terminal_text(terminal: &Terminal<TestBackend>) -> String {
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
fn favorite_targets_only_selected_track_and_respects_input_ownership() {
    let mut state = TuiSnapshot::default();
    state.queue.current_id = Some(17);
    let mut ui = UiState::default();
    ui.queue.set_viewport(2);
    ui.queue.insert(
        0,
        2,
        2,
        vec![
            QueueRow {
                id: 17,
                track_id: 41,
                position: 0,
                offset: 0,
                track: track(41),
            },
            QueueRow {
                id: 18,
                track_id: 42,
                position: 1,
                offset: 1,
                track: track(42),
            },
        ],
    );
    ui.queue.selection.select(Some(1));
    for _ in 0..2 {
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char('m')), &state),
            KeyAction::Command(Command::ToggleFavorite { track_ids }) if track_ids == vec![42]
        ));
    }
    for key in [KeyCode::F(1), KeyCode::Char('i')] {
        ui.key_action(KeyEvent::from(key), &state);
        assert!(matches!(
            ui.key_action(KeyEvent::from(KeyCode::Char('m')), &state),
            KeyAction::Ignored
        ));
        ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    }
    ui.open_search();
    ui.key_action(KeyEvent::from(KeyCode::Char('m')), &state);
    assert_eq!(ui.search.query, "m");
    ui.clear_search(&state);
    ui.view = View::Playlists(PagedList::default());
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('m')), &state),
        KeyAction::Ignored
    ));
    let mut tree = LibraryTree::default();
    tree.entries.set_viewport(1);
    tree.entries.insert(
        0,
        1,
        1,
        vec![DirectoryRow::Directory {
            path: "/music".into(),
        }],
    );
    ui.view = View::Tree(tree);
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char('m')), &state),
        KeyAction::Ignored
    ));
}

#[test]
fn details_pin_search_target_reject_late_reads_and_restore_source() {
    let state = query_snapshot();
    let mut ui = UiState {
        search: SearchState {
            query: "夜航".into(),
            applied_query: "夜航".into(),
            active: true,
            editing: false,
        },
        ..UiState::default()
    };
    let mut results = PagedList::default();
    results.set_viewport(2);
    results.insert(0, 2, 2, vec![track(1), track(2)]);
    results.selection.select(Some(1));
    ui.view = View::LibraryResults(results);
    ui.key_action(KeyEvent::from(KeyCode::Char('i')), &state);
    assert!(ui.details_active);
    let old = ui.query_request(&state).unwrap();
    assert_eq!(old.kind, QueryKind::Track(2));
    for key in [
        KeyCode::Delete,
        KeyCode::Enter,
        KeyCode::Char('m'),
        KeyCode::Char('i'),
        KeyCode::Char(' '),
        KeyCode::Tab,
        KeyCode::BackTab,
    ] {
        assert!(matches!(
            ui.key_action(KeyEvent::from(key), &state),
            KeyAction::Ignored
        ));
        assert!(ui.details_active);
    }
    assert!(ui.apply_query(
        &state,
        &old,
        Ok(QueryResult {
            revisions: state.query_revisions(None).for_query(
                &old.query(),
                &state.library.playlist_entry_revisions,
                &state.library.metadata_revisions
            ),
            view: ViewResponse::Track(Some(full_track(2)))
        })
    ));
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
    ui.key_action(KeyEvent::from(KeyCode::End), &state);
    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
    assert!(matches!(&ui.details.state, DetailState::Ready(track) if track.id == 2));
    assert_eq!(ui.details.scroll, ui.details.max_scroll);
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    let View::LibraryResults(results) = &ui.view else {
        panic!("search source lost")
    };
    assert_eq!(ui.search.applied_query, "夜航");
    assert!(!ui.search.editing);
    assert_eq!(results.selected().unwrap().id, 2);
    ui.key_action(KeyEvent::from(KeyCode::Up), &state);
    ui.key_action(KeyEvent::from(KeyCode::Char('i')), &state);
    let current = ui.query_request(&state).unwrap();
    assert_eq!(current.kind, QueryKind::Track(1));
    assert!(!ui.apply_query(
        &state,
        &old,
        Ok(QueryResult {
            revisions: state.query_revisions(None),
            view: ViewResponse::Track(None)
        })
    ));
    assert!(ui.apply_query(
        &state,
        &current,
        Ok(QueryResult {
            revisions: state.query_revisions(None).for_query(
                &current.query(),
                &state.library.playlist_entry_revisions,
                &state.library.metadata_revisions
            ),
            view: ViewResponse::Track(None)
        })
    ));
    assert!(matches!(ui.details.state, DetailState::Removed));
    ui.details.stamp = None;
    let failed = ui.query_request(&state).unwrap();
    assert!(ui.apply_query(
        &state,
        &failed,
        Err(QueryError::Message("read failed".into()))
    ));
    assert!(matches!(&ui.details.state, DetailState::Error(error) if error == "read failed"));
    assert!(ui.query_request(&state).is_none());
}

pub(in crate::terminal) fn full_track(id: i64) -> rivu_protocol::model::Track {
    rivu_protocol::model::Track {
        id,
        path: "/music/夜航.flac".into(),
        fingerprint: None,
        cue: None,
        title: "夜航 e\u{301}".repeat(12),
        artist: "Artist".repeat(15),
        album: "Album".repeat(15),
        duration: Some(125.0),
        codec: "flac".into(),
        channels: 2,
        sample_rate: 96000,
        bitrate_bps: Some(1200000),
        bitrate_exact: false,
        track_number: Some(3),
        disc_number: Some(1),
        bits_per_sample: Some(24),
        release_date: Some("2026-10".into()),
        favorite: true,
        decoder_hint: rivu_format::decoder_hint::DecoderHint::Native,
        missing: true,
        play_count: 0,
        last_played: None,
    }
}

#[test]
fn notices_preserve_actionable_failures_across_operation_feedback() {
    let mut ui = UiState::default();
    ui.set_notice("failure".into(), NoticeKind::Error);
    ui.set_notice("Added to queue".into(), NoticeKind::Information);
    assert!(matches!(
        ui.message.as_ref().unwrap().kind,
        NoticeKind::Error
    ));
    ui.message.as_mut().unwrap().expires_at = Instant::now();
    ui.set_notice("Added to queue".into(), NoticeKind::Information);
    assert!(matches!(
        ui.message.as_ref().unwrap().kind,
        NoticeKind::Information
    ));
}

#[test]
fn playback_transitions_and_track_changes_leave_footer_notices_untouched() {
    let mut tui = TuiState::new(
        TuiSnapshot::default(),
        Arc::new(Localizer::for_terminal("en-US")),
    );
    for status in [
        PlaybackStatus::Playing,
        PlaybackStatus::Paused,
        PlaybackStatus::Stopped,
    ] {
        let mut next = tui.snapshot.clone();
        next.playback.status = status;
        next.queue.current_id = Some(next.queue.current_id.unwrap_or(0) + 1);
        next.current_track = Some(Arc::new(full_track(1)));
        tui.apply_snapshot(next);
        assert!(tui.ui.message.is_none());
    }
    tui.ui.set_notice("failure".into(), NoticeKind::Error);
    let deadline = tui.ui.message.as_ref().unwrap().expires_at;
    let mut next = tui.snapshot.clone();
    next.playback.status = PlaybackStatus::Playing;
    tui.apply_snapshot(next);
    let notice = tui.ui.message.as_ref().unwrap();
    assert_eq!(notice.text, "failure");
    assert_eq!(notice.expires_at, deadline);
}

#[test]
fn queue_pages_keep_global_navigation_and_duplicate_occurrence_actions() {
    let mut state = TuiSnapshot::default();
    state.queue.total = 200_000;
    state.system.query_batch_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    state.system.query_batch_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    assert!(tui.ui.apply_query(
        &state,
        &request,
        Err(QueryError::Message("queue unavailable".into()))
    ));
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

pub(in crate::terminal) fn queue_row(position: usize, id: u64) -> QueueRow {
    QueueRow {
        id,
        track_id: 7,
        position,
        offset: position,
        track: track(7),
    }
}

pub(in crate::terminal) fn load_queue(ui: &mut UiState, state: &TuiSnapshot) {
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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

pub(in crate::terminal) fn track(index: usize) -> LibraryRow {
    LibraryRow {
        path: Default::default(),
        id: index as i64,
        title: format!("row-{index}"),
        artist: "artist".into(),
        album: "album".into(),
        duration: None,
        codec: "flac".into(),
        channels: 2,
        sample_rate: 44100,
        bits_per_sample: None,
        cue_number: None,
        bitrate_bps: None,
        bitrate_exact: false,
        favorite: false,
        missing: false,
        play_count: 0,
    }
}

pub(in crate::terminal) fn response(request: &QueryRequest, total: usize) -> QueryResult {
    let indices = request.offset..(request.offset + request.limit).min(total);
    let view = match &request.kind {
        QueryKind::OtherTracksCount => ViewResponse::OtherTracksCount(total),
        QueryKind::LibrarySelection(track_id) | QueryKind::DirectorySelection { track_id, .. } => {
            ViewResponse::LibraryLocate(rivu_protocol::model::LibraryLocateResult::Located(
                rivu_protocol::model::LibraryLocation {
                    offset: *track_id as usize,
                    total,
                    child: None,
                },
            ))
        }
        QueryKind::Track(_) => panic!("track reads need explicit metadata"),
        QueryKind::PlaylistSummary(playlist_id) => {
            ViewResponse::PlaylistSummary(Some(PlaylistSummary {
                id: *playlist_id,
                name: "collection".into(),
                entry_count: total,
            }))
        }
        QueryKind::Queue => ViewResponse::QueuePage(Page {
            total,
            rows: indices
                .map(|position| queue_row(position, position as u64 + 1000))
                .collect(),
        }),
        QueryKind::QueueSelection(_) => {
            panic!("selection lookups require an explicit position")
        }
        QueryKind::Search(_) | QueryKind::OtherTracks { .. } => ViewResponse::LibraryPage(Page {
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
                    path: Default::default(),
                    channels: 2,
                    sample_rate: 44100,
                    bits_per_sample: None,
                    bitrate_bps: None,
                    bitrate_exact: false,
                    cue_number: None,
                    favorite: false,
                    missing: false,
                })
                .collect(),
        }),
        QueryKind::PlaylistSelection {
            playlist_id,
            entry_id,
        } => ViewResponse::PlaylistEntryLocations(vec![
            rivu_protocol::model::PlaylistEntryLocation {
                id: *entry_id,
                playlist_id: *playlist_id,
                position: *entry_id as usize,
                offset: *entry_id as usize,
                track_id: *entry_id,
                total,
            },
        ]),
    };
    QueryResult {
        revisions: request.stamp.revisions(),
        view,
    }
}

#[test]
fn other_header_tracks_unfiltered_membership_and_rejects_stale_results() {
    for roots in [vec![], vec![PathBuf::from("/music")]] {
        let mut state = query_snapshot();
        state.system.library_roots = Arc::new(roots.clone());
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
        tui.ui.view = View::Tree(LibraryTree::new(&roots));
        render_view(&mut tui.ui);
        tui.ui.prepare_local_view(&state);
        let first = tui.ui.query_request(&state).unwrap();
        assert!(tui.ui.apply_query(&state, &first, Ok(response(&first, 0))));
        let View::Tree(tree) = &tui.ui.view else {
            unreachable!()
        };
        assert_eq!(tree.entries.total, roots.len());
        assert!(tui.ui.query_request(&state).is_none());

        state.library.structure_revision += 1;
        tui.apply_snapshot(state.clone());
        let stale = tui.ui.query_request(&state).unwrap();
        state.library.structure_revision += 1;
        tui.apply_snapshot(state.clone());
        assert!(!tui.ui.apply_query(&state, &stale, Ok(response(&stale, 0))));
        let current = tui.ui.query_request(&state).unwrap();
        assert!(
            tui.ui
                .apply_query(&state, &current, Ok(response(&current, 1)))
        );
        let View::Tree(tree) = &tui.ui.view else {
            unreachable!()
        };
        assert_eq!(tree.entries.total, roots.len() + 1);

        state.library.structure_revision += 1;
        tui.apply_snapshot(state.clone());
        let current = tui.ui.query_request(&state).unwrap();
        assert!(
            tui.ui
                .apply_query(&state, &current, Ok(response(&current, 0)))
        );
        let View::Tree(tree) = &tui.ui.view else {
            unreachable!()
        };
        assert_eq!(tree.entries.total, roots.len());
        assert!(tui.ui.query_request(&state).is_none());
    }
}

#[test]
fn other_header_selection_survives_membership_refresh() {
    let mut state = query_snapshot();
    state.system.library_roots = Arc::new(vec![PathBuf::from("/music")]);
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = View::Tree(LibraryTree::new(&state.system.library_roots));
    render_view(&mut tui.ui);
    load_visible(&mut tui.ui, &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    state.library.revision += 1;
    tui.apply_snapshot(state.clone());
    assert!(tui.ui.query_request(&state).is_none());
    state.library.structure_revision += 1;
    tui.apply_snapshot(state.clone());
    load_visible(&mut tui.ui, &state);
    let View::Tree(tree) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(tree.entries.selected(), Some(&DirectoryRow::OtherTracks));
}

pub(in crate::terminal) fn load_visible(ui: &mut UiState, state: &TuiSnapshot) {
    ui.prepare_local_view(state);
    for _ in 0..16 {
        let Some(request) = ui.query_request(state) else {
            return;
        };
        assert!(ui.apply_query(state, &request, Ok(response(&request, 12))));
    }
    panic!("visible requests did not converge");
}

pub(in crate::terminal) fn playlist_view(playlist_id: i64) -> View {
    View::PlaylistDetail {
        playlists: (ListState::default(), 1),
        detail: PlaylistDetail {
            playlist_id,
            name: "collection".into(),
            summary_dirty: false,
            entries: PagedList::default(),
            entry_anchor: None,
            selected_entry_id: None,
        },
    }
}

pub(in crate::terminal) fn query_snapshot() -> TuiSnapshot {
    let mut state = TuiSnapshot::default();
    state.system.query_batch_size = 3;
    state.queue.total = 12;
    state.queue.revision = 5;
    state.library.revision = 6;
    state.library.structure_revision = 7;
    state.library.playlist_revision = 8;
    Arc::make_mut(&mut state.library.playlist_entry_revisions).insert(1, 9);
    state
}

fn recent_entry(id: i64, played_at: i64) -> HistoryEntry {
    HistoryEntry {
        track_id: id,
        title: format!("recent-{id}"),
        play_count: 1,
        played_at,
        favorite: false,
    }
}

fn recent_ids(recent: &RecentTracks) -> Vec<i64> {
    (0..recent.entries.total)
        .map(|position| recent.source[*recent.entries.get(position).unwrap()].track_id)
        .collect()
}

#[test]
fn recent_playback_overrides_time_and_breaks_ties_before_source_order() {
    let mut state = query_snapshot();
    state.library.history = Arc::new(vec![
        recent_entry(90, 20),
        recent_entry(4, 20),
        recent_entry(60, 20),
    ]);
    state.current_track = Some(Arc::new(full_track(60)));
    state.playback.status = PlaybackStatus::Playing;
    state.playback.last_heard_at = Some(20);
    let mut recent = RecentTracks::default();
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [60, 90, 4]);
    assert_eq!(recent.played_at(&recent.source[2]), 20);

    state.playback.status = PlaybackStatus::Paused;
    state.playback.last_heard_at = Some(10);
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [90, 4, 60]);
    assert_eq!(recent.played_at(&recent.source[2]), 10);
    assert_eq!(recent.source[2].played_at, 20);
    assert_eq!(recent.source[2].play_count, 1);

    state.playback.last_heard_at = Some(30);
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [60, 90, 4]);
    state.playback.status = PlaybackStatus::Stopped;
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [90, 4, 60]);
    assert_eq!(recent.played_at(&recent.source[2]), 20);

    state.playback.status = PlaybackStatus::Playing;
    state.playback.last_heard_at = None;
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [90, 4, 60]);
    state.playback.last_heard_at = Some(30);
    state.current_track = None;
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [90, 4, 60]);
}

#[test]
fn recent_track_switch_reorders_both_playback_rows_and_preserves_selection() {
    let mut state = query_snapshot();
    state.library.history = Arc::new(vec![
        recent_entry(9, 40),
        recent_entry(4, 30),
        recent_entry(7, 20),
        recent_entry(2, 30),
    ]);
    state.current_track = Some(Arc::new(full_track(7)));
    state.playback.status = PlaybackStatus::Playing;
    state.playback.last_heard_at = Some(50);
    let mut recent = RecentTracks::default();
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [7, 9, 4, 2]);
    recent.entries.selection.select(Some(2));

    state.current_track = Some(Arc::new(full_track(2)));
    state.playback.last_heard_at = Some(40);
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [2, 9, 4, 7]);
    assert_eq!(recent.selected().unwrap().track_id, 4);
    state.playback.status = PlaybackStatus::Stopped;
    recent.sync(&state, "");
    assert_eq!(recent_ids(&recent), [9, 4, 2, 7]);
    assert_eq!(recent.selected().unwrap().track_id, 4);
    assert_eq!(recent.entries.selection.selected(), Some(1));
}

#[test]
fn recent_playback_ticks_keep_filtered_selection_and_history_limit() {
    let mut state = query_snapshot();
    state.library.history = Arc::new((1..=257).map(|id| recent_entry(id, id)).collect());
    state.current_track = Some(Arc::new(full_track(256)));
    state.playback.status = PlaybackStatus::Playing;
    state.playback.last_heard_at = Some(300);
    let mut recent = RecentTracks::default();
    recent.sync(&state, "recent-2");
    assert_eq!(recent.entries.total, 68);
    assert!(!recent_ids(&recent).contains(&257));
    recent.entries.set_viewport(4);
    recent.entries.selection.select(Some(5));
    recent.entries.set_viewport(4);
    let selected = recent.selected().unwrap().track_id;
    let offset = recent.entries.selection.offset();
    for time in [301, 319] {
        state.playback.last_heard_at = Some(time);
        recent.sync(&state, "recent-2");
        assert_eq!(recent.entries.selection.offset(), offset);
        assert_eq!(recent.selected().unwrap().track_id, selected);
        assert_eq!(recent.played_at(&recent.source[255]), time);
    }
    state.current_track = Some(Arc::new(full_track(257)));
    recent.sync(&state, "recent-2");
    assert_eq!(recent.entries.total, 68);
    assert!(!recent_ids(&recent).contains(&257));
    assert!(recent.source.iter().all(|entry| entry.play_count == 1));
}

#[test]
fn recent_search_clear_and_cached_playback_updates_follow_selection_rules() {
    let mut state = query_snapshot();
    let mut first = recent_entry(1, 10);
    first.title = "keep first".into();
    let mut second = recent_entry(2, 20);
    second.title = "keep second".into();
    state.library.history = Arc::new(vec![first, second, recent_entry(3, 30)]);
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    tui.ui.open_search();
    tui.ui.search.query = "KEEP".into();
    tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);

    state.current_track = Some(Arc::new(full_track(1)));
    state.playback.status = PlaybackStatus::Playing;
    state.playback.last_heard_at = Some(40);
    tui.apply_snapshot(state.clone());
    let View::Recent(recent) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(recent_ids(recent), [1, 2]);
    assert_eq!(recent.selected().unwrap().track_id, 1);
    assert_eq!(recent.entries.selection.selected(), Some(0));

    state.playback.status = PlaybackStatus::Stopped;
    tui.apply_snapshot(state.clone());
    let View::Recent(recent) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(recent_ids(recent), [2, 1]);
    assert_eq!(recent.selected().unwrap().track_id, 1);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    let Some(View::Recent(recent)) = &tui.ui.cached_recent else {
        unreachable!()
    };
    assert_eq!(recent_ids(recent), [3, 2, 1]);
    assert_eq!(recent.selected().unwrap().track_id, 1);

    for (id, time, expected) in [(2, 50, [2, 3, 1]), (3, 60, [3, 2, 1])] {
        state.current_track = Some(Arc::new(full_track(id)));
        state.playback.status = PlaybackStatus::Playing;
        state.playback.last_heard_at = Some(time);
        tui.apply_snapshot(state.clone());
        let Some(View::Recent(recent)) = &tui.ui.cached_recent else {
            unreachable!()
        };
        assert_eq!(recent_ids(recent), expected);
        assert_eq!(recent.selected().unwrap().track_id, 1);
        assert_eq!(recent.played_at(&recent.source[(id - 1) as usize]), time);
    }
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    let View::Recent(recent) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(recent_ids(recent), [3, 2, 1]);
    assert_eq!(recent.selected().unwrap().track_id, 1);
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
        KeyAction::Command(Command::Play { track_id: 1 })
    ));
}

#[test]
fn recent_refresh_preserves_selected_track_across_reorder_and_hidden_updates() {
    let mut state = query_snapshot();
    state.library.history = Arc::new(vec![
        recent_entry(1, 10),
        recent_entry(2, 20),
        recent_entry(3, 30),
    ]);
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    let View::Recent(recent) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(recent.selected().unwrap().track_id, 3);
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    state.library.history = Arc::new(vec![
        recent_entry(1, 50),
        recent_entry(2, 20),
        recent_entry(3, 30),
    ]);
    tui.apply_snapshot(state.clone());
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    let View::Recent(recent) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(recent.selected().unwrap().track_id, 2);
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
        KeyAction::Command(Command::Play { track_id: 2 })
    ));
    assert!(
        matches!(tui.ui.key_action(KeyEvent::from(KeyCode::Char('m')), &state), KeyAction::Command(Command::ToggleFavorite { track_ids }) if track_ids == vec![2])
    );
    tui.ui
        .key_action(KeyEvent::from(KeyCode::Char('i')), &state);
    assert_eq!(
        tui.ui.query_request(&state).unwrap().kind,
        QueryKind::Track(2)
    );
    tui.ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    state.library.history = Arc::new(vec![recent_entry(1, 50), recent_entry(3, 30)]);
    tui.apply_snapshot(state.clone());
    let View::Recent(recent) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(recent.selected().unwrap().track_id, 3);
    state.library.history = Arc::new(vec![]);
    tui.apply_snapshot(state.clone());
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
        KeyAction::Ignored
    ));
    assert!(tui.ui.query_request(&state).is_none());
    assert_eq!(tui.ui.view.tab(), TopTab::Recent);
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
                [
                    TopTab::Recent,
                    TopTab::Library,
                    TopTab::Playlists,
                    TopTab::Queue,
                ]
            } else {
                [
                    TopTab::Playlists,
                    TopTab::Library,
                    TopTab::Recent,
                    TopTab::Queue,
                ]
            };
            for tab in expected.into_iter().cycle().take(8) {
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    assert_eq!(
        (
            tui.ui.queue.selection.selected(),
            tui.ui.queue.selection.offset()
        ),
        queue_position
    );
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    load_visible(&mut tui.ui, &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    load_visible(&mut tui.ui, &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    tui.ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    let old = tui.ui.query_request(&state).unwrap();
    let mut next = state.clone();
    next.system.query_batch_size = 5;
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
    tui.ui.key_action(KeyEvent::from(KeyCode::BackTab), &next);
    load_visible(&mut tui.ui, &next);
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &next);
    let View::Tree(tree) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(tree.roots, vec![PathBuf::from("/two")]);
    assert_eq!(tree.entries.total, 2);
    assert_eq!(tree.entries.selected(), Some(&DirectoryRow::OtherTracks));
    tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &next);
    assert!(matches!(
        tui.ui.query_request(&next).unwrap().kind,
        QueryKind::OtherTracks { .. }
    ));
    tui.ui.key_action(KeyEvent::from(KeyCode::Left), &next);
    load_visible(&mut tui.ui, &next);
    tui.ui.key_action(KeyEvent::from(KeyCode::Up), &next);
    load_visible(&mut tui.ui, &next);
    let View::Tree(tree) = &tui.ui.view else {
        unreachable!()
    };
    assert!(
        matches!(tree.entries.selected(), Some(DirectoryRow::Directory { path }) if path == Path::new("/two"))
    );
    tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &next);
    assert_eq!(
        tui.ui.query_request(&next).unwrap().kind,
        QueryKind::Directory("/two".into())
    );
}

#[test]
fn search_tab_traversal_clears_editor_and_keeps_directory_scope() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: View::Tree(LibraryTree {
            directory: "/music/disc".into(),
            ..LibraryTree::default()
        }),
        ..UiState::default()
    };
    ui.open_search();
    ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
    assert_eq!(ui.view.tab(), TopTab::Recent);
    assert!(!ui.search.active);
    ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    assert!(matches!(&ui.view, View::Tree(tree) if tree.directory == Path::new("/music/disc")));
}

#[test]
fn unrelated_updates_preserve_pending_and_loaded_terminal_pages() {
    for view in [
        View::Queue,
        View::LibraryResults(PagedList::default()),
        View::Tree(LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        }),
        View::Playlists(PagedList::default()),
        playlist_view(1),
    ] {
        let state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
                next.library.playlist_revision = state.library.playlist_revision;
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
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = View::Playlists(PagedList::default());
    let first = tui.ui.query_request(&state).unwrap();
    assert!(tui.ui.apply_query(&state, &first, Ok(response(&first, 12))));
    let second = tui.ui.query_request(&state).unwrap();
    assert_ne!(first.offset, second.offset);
    assert_eq!(first.generation, second.generation);
    assert_eq!(first.stamp, second.stamp);
    let mut next = state.clone();
    next.system.query_batch_size = 5;
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
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = View::Playlists(PagedList::default());
    let request = tui.ui.query_request(&state).unwrap();
    assert!(tui.ui.apply_query(
        &state,
        &request,
        Err(QueryError::Message("unavailable".into()))
    ));
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
    snapshot.system.query_batch_size = 3;
    Arc::make_mut(&mut snapshot.library.playlist_entry_revisions).insert(2, 0);
    for view in [
        View::LibraryResults(PagedList::default()),
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
                summary_dirty: false,
                entries: PagedList::default(),
                entry_anchor: None,
                selected_entry_id: None,
            },
        },
    ] {
        let mut tui = TuiState::new(snapshot.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
    snapshot.system.query_batch_size = 1;
    Arc::make_mut(&mut snapshot.library.playlist_entry_revisions).insert(1, 0);
    let mut tui = TuiState::new(snapshot, Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = View::PlaylistDetail {
        playlists: (ListState::default(), 1),
        detail: PlaylistDetail {
            playlist_id: 1,
            name: "one".into(),
            summary_dirty: false,
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
    entries[0].offset = 9;
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
    state.system.query_batch_size = 3;
    Arc::make_mut(&mut state.library.playlist_entry_revisions).insert(7, 0);
    let views = [
        View::LibraryResults(PagedList::default()),
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
                summary_dirty: false,
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

fn other_tracks_pending_selection_regression(revisions: bool) {
    let mut state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    let mut tree = LibraryTree {
        other_tracks: true,
        ..LibraryTree::default()
    };
    tree.entries.set_viewport(3);
    tree.entries.insert(
        0,
        3,
        3,
        vec![41, 42, 43]
            .into_iter()
            .map(|id| DirectoryRow::Track(track(id)))
            .collect(),
    );
    tree.entries.selection.select(Some(1));
    tui.ui.view = View::Tree(tree);
    if revisions {
        state.library.revision += 1;
        tui.apply_snapshot(state.clone());
    } else {
        tui.ui
            .key_action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT), &state);
    }
    let old = tui.ui.query_request(&state).unwrap();
    assert!(matches!(
        old.kind,
        QueryKind::OtherTracks {
            anchor: Some(42),
            ..
        }
    ));
    if revisions {
        state.library.revision += 1;
        tui.apply_snapshot(state.clone());
    } else {
        tui.ui
            .key_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT), &state);
    }
    let fresh = tui.ui.query_request(&state).unwrap();
    assert!(matches!(
        fresh.kind,
        QueryKind::OtherTracks {
            anchor: Some(42),
            ..
        }
    ));
    assert_ne!(old, fresh);
    let located = |offset| QueryResult {
        revisions: state.query_revisions(None),
        view: ViewResponse::LibraryLocate(rivu_protocol::model::LibraryLocateResult::Located(
            rivu_protocol::model::LibraryLocation {
                offset,
                total: 3,
                child: None,
            },
        )),
    };
    assert!(!tui.ui.apply_query(&state, &old, Ok(located(2))));
    assert_eq!(tui.ui.query_request(&state), Some(fresh.clone()));
    assert!(tui.ui.apply_query(&state, &fresh, Ok(located(0))));
    let rows = tui.ui.query_request(&state).unwrap();
    assert!(matches!(
        rows.kind,
        QueryKind::OtherTracks { anchor: None, .. }
    ));
    assert!(tui.ui.apply_query(
        &state,
        &rows,
        Ok(QueryResult {
            revisions: state.query_revisions(None),
            view: ViewResponse::LibraryPage(Page {
                total: 3,
                rows: vec![track(42), track(43), track(41)],
            }),
        }),
    ));
    let View::Tree(tree) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(tree.entries.selection.selected(), Some(0));
    assert_eq!(
        tree.entries.selected(),
        Some(&DirectoryRow::Track(track(42)))
    );
    assert!(!tui.ui.apply_query(&state, &old, Ok(located(2))));
}

#[test]
fn other_tracks_selection_survives_rapid_sort_and_filter_changes() {
    other_tracks_pending_selection_regression(false);
}

#[test]
fn other_tracks_selection_survives_consecutive_revisions_before_locate() {
    other_tracks_pending_selection_regression(true);
}

#[test]
fn other_tracks_selection_survives_changes_between_locate_and_page() {
    for revision_change in [false, true] {
        let mut state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
        let mut tree = LibraryTree {
            other_tracks: true,
            ..LibraryTree::default()
        };
        tree.entries.insert(
            0,
            3,
            3,
            vec![41, 42, 43]
                .into_iter()
                .map(|id| DirectoryRow::Track(track(id)))
                .collect(),
        );
        tree.entries.selection.select(Some(1));
        tui.ui.view = View::Tree(tree);
        tui.ui
            .key_action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT), &state);
        let locate = tui.ui.query_request(&state).unwrap();
        let located = |request: &QueryRequest, offset| QueryResult {
            revisions: request.stamp.revisions(),
            view: ViewResponse::LibraryLocate(rivu_protocol::model::LibraryLocateResult::Located(
                rivu_protocol::model::LibraryLocation {
                    offset,
                    total: 3,
                    child: None,
                },
            )),
        };
        assert!(tui.ui.apply_query(&state, &locate, Ok(located(&locate, 0))));
        let old_page = tui.ui.query_request(&state).unwrap();
        if revision_change {
            state.library.revision += 1;
            tui.apply_snapshot(state.clone());
        } else {
            tui.ui
                .key_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT), &state);
        }
        let fresh = tui.ui.query_request(&state).unwrap();
        assert!(matches!(
            fresh.kind,
            QueryKind::OtherTracks {
                anchor: Some(42),
                ..
            }
        ));
        let page = |request: &QueryRequest| QueryResult {
            revisions: request.stamp.revisions(),
            view: ViewResponse::LibraryPage(Page {
                total: 3,
                rows: vec![track(41), track(43), track(42)],
            }),
        };
        assert!(!tui.ui.apply_query(&state, &old_page, Ok(page(&old_page))));
        assert!(tui.ui.apply_query(&state, &fresh, Ok(located(&fresh, 2))));
        let rows = tui.ui.query_request(&state).unwrap();
        assert!(tui.ui.apply_query(&state, &rows, Ok(page(&rows))));
        assert!(matches!(
            tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
            KeyAction::Command(Command::Play { track_id: 42 })
        ));
        tui.ui.key_action(KeyEvent::from(KeyCode::Up), &state);
        tui.ui
            .key_action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT), &state);
        assert!(matches!(
            tui.ui.query_request(&state).unwrap().kind,
            QueryKind::OtherTracks {
                anchor: Some(43),
                ..
            }
        ));
    }
}

#[test]
fn tree_top_level_has_configured_roots_then_other_and_restores_parent() {
    let mut state = TuiSnapshot::default();
    state.system.query_batch_size = 1;
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
        assert_eq!(tree.entries.total, roots.len() + 1);
        assert!(tree.entries.loaded);
        let mut ordered_roots = roots.clone();
        ordered_roots.sort();
        let expected = ordered_roots
            .into_iter()
            .map(|path| DirectoryRow::Directory { path })
            .chain(std::iter::once(DirectoryRow::OtherTracks));
        for (index, row) in expected.enumerate() {
            load_visible(&mut ui, &state);
            let View::Tree(tree) = &ui.view else {
                unreachable!()
            };
            assert_eq!(tree.entries.selection.selected(), Some(index));
            assert_eq!(tree.entries.selected(), Some(&row));
            ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
            let request = ui.query_request(&state).unwrap();
            match &row {
                DirectoryRow::OtherTracks => {
                    assert!(matches!(request.kind, QueryKind::OtherTracks { .. }));
                }
                DirectoryRow::Directory { path } => {
                    assert_eq!(request.kind, QueryKind::Directory(path.clone()));
                }
                DirectoryRow::Track(_) => unreachable!(),
            }
            ui.key_action(KeyEvent::from(KeyCode::Left), &state);
            load_visible(&mut ui, &state);
            let View::Tree(tree) = &ui.view else {
                unreachable!()
            };
            assert!(!tree.other_tracks);
            assert!(tree.directory.as_os_str().is_empty());
            assert_eq!(tree.entries.selection.selected(), Some(index));
            assert_eq!(tree.entries.selected(), Some(&row));
            assert!(ui.query_request(&state).is_none());
            ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        }
    }
}

#[test]
fn stale_revisions_do_not_stall_navigation() {
    let mut state = TuiSnapshot::default();
    state.system.query_batch_size = 3;
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
    ui.key_action(KeyEvent::from(KeyCode::Backspace), &state);
    assert!(matches!(&ui.view, View::Tree(tree) if tree.directory == Path::new("/music/34")));
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
            search: SearchState {
                active: true,
                editing: true,
                ..SearchState::default()
            },
            ..UiState::default()
        },
        UiState {
            show_help: true,
            ..UiState::default()
        },
        UiState {
            details_active: true,
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
        ui.open_search();
        ui.key_action(help, &state);
        assert!(!ui.show_help);
        assert_eq!(ui.search.query, "?");
        ui.key_action(KeyEvent::from(KeyCode::Tab), &state);
        ui.key_action(help, &state);
        assert!(ui.show_help);
    }
}

#[test]
fn query_editor_keeps_printable_shortcuts_and_rejects_modified_text() {
    let state = TuiSnapshot::default();
    let mut ui = UiState::default();
    ui.open_search();
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
    assert_eq!(ui.search.query, "R? q Aa/[]!春");
    ui.key_action(KeyEvent::from(KeyCode::BackTab), &state);
    assert!(!ui.search.active);
    assert_eq!(ui.view.tab(), TopTab::Recent);
    assert!(matches!(
        ui.key_action(KeyEvent::from(KeyCode::Char(' ')), &state),
        KeyAction::Command(Command::Toggle)
    ));
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    assert!(matches!(ui.view, View::Queue));
}

#[test]
fn help_scrolls_every_binding_without_touching_the_underlying_queue() {
    for language in ["en-US", "zh-CN"] {
        let state = TuiSnapshot::default();
        let mut ui = UiState::new(Arc::new(Localizer::for_terminal(language)));
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
    state.system.query_batch_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
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
        playback: rivu_protocol::model::PlaybackState {
            status: PlaybackStatus::Playing,
            position: 12.0,
            ..Default::default()
        },
        queue: rivu_protocol::model::QueueState {
            current_id: Some(7),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut ui = UiState::default();
    assert!(matches!(
        ui.key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL), &state),
        KeyAction::Command(Command::SeekRelative {
            queue_id: 7,
            delta: -5.0
        })
    ));
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

#[test]
fn details_containers_are_not_tracks_and_query_editor_owns_i() {
    let state = query_snapshot();
    let mut ui = UiState::default();
    let mut playlists = PagedList::default();
    playlists.set_viewport(1);
    playlists.insert(
        0,
        1,
        1,
        vec![PlaylistSummary {
            id: 9,
            name: "Full playlist name 夜航".into(),
            entry_count: 2,
        }],
    );
    ui.view = View::Playlists(playlists);
    ui.key_action(KeyEvent::from(KeyCode::Char('i')), &state);
    assert!(ui.details_active);
    assert!(ui.details.target.is_none());
    assert!(
        matches!(&ui.details.state, DetailState::Container(name) if name == "Full playlist name 夜航")
    );
    assert!(ui.query_request(&state).is_none());
    ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
    ui.open_search();
    ui.key_action(KeyEvent::from(KeyCode::Char('i')), &state);
    assert!(!ui.details_active);
    assert_eq!(ui.search.query, "i");
}

fn bitrate_tui() -> TuiState {
    let mut state = query_snapshot();
    state.queue.total = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.queue.set_viewport(2);
    tui.ui.queue.insert(
        0,
        3,
        3,
        (0..3)
            .map(|position| queue_row(position, position as u64 + 1000))
            .collect(),
    );
    tui.ui.queue.selection.select(Some(1));
    *tui.ui.queue.selection.offset_mut() = 1;

    tui.ui.search.query = "draft query".into();
    let mut results = PagedList::default();
    results.set_viewport(2);
    results.insert(0, 3, 12, vec![track(7), track(8), track(7)]);
    results.selection.select(Some(1));
    tui.ui.view = View::LibraryResults(results);

    let mut tree = LibraryTree {
        directory: "/music".into(),
        ..LibraryTree::default()
    };
    tree.entries.set_viewport(2);
    tree.entries.insert(
        0,
        3,
        3,
        vec![
            DirectoryRow::Directory {
                path: "/music/child".into(),
            },
            DirectoryRow::Track(track(7)),
            DirectoryRow::Track(track(7)),
        ],
    );
    tree.entries.selection.select(Some(2));
    *tree.entries.selection.offset_mut() = 1;
    tui.ui.cached_library = Some(View::Tree(tree));

    let mut playlist = UiState {
        view: playlist_view(1),
        ..UiState::default()
    };
    let request = playlist.query_request(&state).unwrap();
    assert!(playlist.apply_query(&state, &request, Ok(response(&request, 3))));
    let View::PlaylistDetail { detail, .. } = &mut playlist.view else {
        unreachable!()
    };
    for entry in detail.entries.pages.values_mut().flatten() {
        entry.track_id = 7;
    }
    detail.entries.selection.select(Some(1));
    detail.selected_entry_id = Some(1);
    tui.ui.cached_playlists = Some(playlist.view);

    let query = Query::Track { track_id: 7 };
    let revisions = state.query_revisions(None).for_query(
        &query,
        &state.library.playlist_entry_revisions,
        &state.library.metadata_revisions,
    );
    tui.ui.details = Details {
        target: Some(7),
        state: DetailState::Ready(Box::new(full_track(7))),
        stamp: Some(query.stamp(revisions)),
        scroll: 4,
        max_scroll: 12,
        ..Details::default()
    };
    tui
}

fn page_coordinates<T>(list: &PagedList<T>) -> (usize, bool, usize, Option<usize>, usize, usize) {
    (
        list.total,
        list.loaded,
        list.fetch_size,
        list.selection.selected(),
        list.selection.offset(),
        list.viewport_rows,
    )
}

#[test]
fn bitrate_late_page_responses_are_rejected_and_retried_at_the_new_stamp() {
    for view in [
        View::Queue,
        View::LibraryResults(PagedList::default()),
        View::Tree(LibraryTree {
            directory: "/music".into(),
            ..LibraryTree::default()
        }),
        View::Tree(LibraryTree {
            other_tracks: true,
            ..LibraryTree::default()
        }),
        playlist_view(1),
    ] {
        let state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
        tui.ui.view = view;
        let pending = tui.ui.query_request(&state).unwrap();
        tui.ui.requested = Some(pending.clone());
        let delayed = response(&pending, 12);
        let mut next = state.clone();
        next.library.bitrate_revision += 1;
        tui.apply_snapshot(next.clone());
        let retry = tui.ui.query_request(&next).unwrap();
        assert_ne!(retry.stamp, pending.stamp);
        assert_ne!(retry.generation, pending.generation);
        tui.ui.requested = Some(retry.clone());
        assert!(!tui.ui.apply_query(&next, &pending, Ok(delayed)));
        assert_eq!(tui.ui.requested.as_ref(), Some(&retry));
        let mut fresh = response(&retry, 12);
        match &mut fresh.view {
            ViewResponse::QueuePage(page) => {
                for row in &mut page.rows {
                    row.track.bitrate_bps = Some(1_536_000);
                    row.track.bitrate_exact = true;
                }
            }
            ViewResponse::LibraryPage(page) => {
                for row in &mut page.rows {
                    row.bitrate_bps = Some(1_536_000);
                    row.bitrate_exact = true;
                }
            }
            ViewResponse::DirectoryPage(page) => {
                for row in &mut page.rows {
                    if let DirectoryRow::Track(track) = row {
                        track.bitrate_bps = Some(1_536_000);
                        track.bitrate_exact = true;
                    }
                }
            }
            ViewResponse::PlaylistEntries(page) => {
                for row in &mut page.rows {
                    row.bitrate_bps = Some(1_536_000);
                    row.bitrate_exact = true;
                }
            }
            _ => unreachable!(),
        }
        assert!(tui.ui.apply_query(&next, &retry, Ok(fresh)));
        let fields = match &tui.ui.view {
            View::Queue => {
                let row = &tui.ui.queue.get(0).unwrap().track;
                (row.bitrate_bps, row.bitrate_exact)
            }
            View::LibraryResults(results) => {
                let row = results.get(0).unwrap();
                (row.bitrate_bps, row.bitrate_exact)
            }
            View::Tree(tree) => {
                let Some(DirectoryRow::Track(row)) = tree.entries.get(0) else {
                    unreachable!()
                };
                (row.bitrate_bps, row.bitrate_exact)
            }
            View::PlaylistDetail { detail, .. } => {
                let row = detail.entries.get(0).unwrap();
                (row.bitrate_bps, row.bitrate_exact)
            }
            View::Playlists(_) | View::Recent(_) => unreachable!(),
        };
        assert_eq!(fields, (Some(1_536_000), true));
    }
}

#[test]
fn bitrate_changes_reload_retained_payloads_without_losing_coordinates_or_identity() {
    let mut tui = bitrate_tui();
    let queue_coordinates = page_coordinates(&tui.ui.queue);
    let View::LibraryResults(results) = &tui.ui.view else {
        unreachable!()
    };
    let search_coordinates = page_coordinates(results);
    let View::Tree(tree) = tui.ui.cached_library.as_ref().unwrap() else {
        unreachable!()
    };
    let tree_coordinates = page_coordinates(&tree.entries);
    let View::PlaylistDetail { detail, .. } = tui.ui.cached_playlists.as_ref().unwrap() else {
        unreachable!()
    };
    let playlist_coordinates = page_coordinates(&detail.entries);
    let pending = tui.ui.query_request(&tui.snapshot).unwrap();
    tui.ui.requested = Some(pending.clone());
    let mut next = tui.snapshot.clone();
    next.library.bitrate_revision += 3;
    tui.apply_snapshot(next.clone());
    assert!(tui.ui.requested.is_none());
    assert_ne!(tui.ui.projection.generation(), pending.generation);
    assert_eq!(page_coordinates(&tui.ui.queue), queue_coordinates);
    assert!(tui.ui.queue.pages.is_empty());
    assert!(tui.ui.queue_anchor.is_none());
    let View::LibraryResults(results) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(page_coordinates(results), search_coordinates);
    assert!(results.pages.is_empty());
    assert_eq!(tui.ui.search.query, "draft query");
    assert!(!tui.ui.search.editing);
    let View::Tree(tree) = tui.ui.cached_library.as_ref().unwrap() else {
        unreachable!()
    };
    assert_eq!(page_coordinates(&tree.entries), tree_coordinates);
    assert_eq!(tree.directory, Path::new("/music"));
    assert!(tree.entries.pages.is_empty());
    let View::PlaylistDetail { detail, .. } = tui.ui.cached_playlists.as_ref().unwrap() else {
        unreachable!()
    };
    assert_eq!(page_coordinates(&detail.entries), playlist_coordinates);
    assert_eq!(
        (
            detail.playlist_id,
            detail.selected_entry_id,
            detail.entry_anchor
        ),
        (1, Some(1), None)
    );
    assert!(detail.entries.pages.is_empty());
    assert!(matches!(tui.ui.details.state, DetailState::Loading));
    assert_eq!(tui.ui.details.target, Some(7));
    assert_eq!((tui.ui.details.scroll, tui.ui.details.max_scroll), (4, 12));
    assert!(tui.ui.details.stamp.is_none());
    assert!(
        !tui.ui
            .apply_query(&next, &pending, Ok(response(&pending, 12)))
    );
    let retry = tui.ui.query_request(&next).unwrap();
    let mut fresh = response(&retry, 12);
    fresh.revisions.bitrate = next.library.bitrate_revision;
    assert!(tui.ui.apply_query(&next, &retry, Ok(fresh)));
    tui.ui.view = tui.ui.cached_library.take().unwrap();
    tui.ui.cancel_query();
    let hidden_tree = tui.ui.query_request(&next).unwrap();
    assert_eq!(hidden_tree.kind, QueryKind::Directory("/music".into()));
    assert_eq!(
        hidden_tree.stamp.revisions().bitrate,
        next.library.bitrate_revision
    );
    assert!(
        tui.ui
            .apply_query(&next, &hidden_tree, Ok(response(&hidden_tree, 3)))
    );
    tui.ui.view = tui.ui.cached_playlists.take().unwrap();
    tui.ui.cancel_query();
    let hidden_playlist = tui.ui.query_request(&next).unwrap();
    assert_eq!(hidden_playlist.kind, QueryKind::Playlist(1));
    assert_eq!(
        hidden_playlist.stamp.revisions().bitrate,
        next.library.bitrate_revision
    );
    assert!(
        tui.ui
            .apply_query(&next, &hidden_playlist, Ok(response(&hidden_playlist, 3)))
    );
}

#[test]
fn bitrate_stale_or_future_response_stamps_retire_and_retry() {
    for baseline in [0, 2] {
        let mut state = query_snapshot();
        state.library.bitrate_revision = 1;
        let mut ui = UiState::default();
        let request = ui.query_request(&state).unwrap();
        ui.requested = Some(request.clone());
        let mut stale = response(&request, 12);
        stale.revisions.bitrate = baseline;
        assert!(!ui.apply_query(&state, &request, Ok(stale)));
        assert!(ui.queue.pages.is_empty());
        assert!(ui.requested.is_none());
        assert!(ui.failed_request.is_none());
        let retry = ui.query_request(&state).unwrap();
        assert_eq!(retry.stamp, request.stamp);
        assert_eq!(retry.generation, request.generation);
        let mut fresh = response(&retry, 12);
        fresh.revisions.bitrate = state.library.bitrate_revision;
        assert!(ui.apply_query(&state, &retry, Ok(fresh)));
    }
}

#[test]
fn bitrate_changes_leave_summary_pages_and_requests_untouched() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = View::Playlists(PagedList::default());
    let first = tui.ui.query_request(&state).unwrap();
    assert!(tui.ui.apply_query(&state, &first, Ok(response(&first, 12))));
    let pending = tui.ui.query_request(&state).unwrap();
    tui.ui.requested = Some(pending.clone());
    let mut next = state.clone();
    next.library.bitrate_revision += 1;
    tui.apply_snapshot(next.clone());
    assert_eq!(tui.ui.requested.as_ref(), Some(&pending));
    assert_eq!(tui.ui.query_request(&next), Some(pending.clone()));
    let View::Playlists(playlists) = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(playlists.get(0).unwrap().name, "row-0");
    assert!(
        tui.ui
            .apply_query(&next, &pending, Ok(response(&pending, 12)))
    );
}

#[test]
fn bitrate_details_reject_late_reads_and_preserve_scroll_during_retry() {
    let mut tui = bitrate_tui();
    tui.ui.details_active = true;
    let old_stamp = tui.ui.details.stamp;
    tui.ui.details.stamp = None;
    let old = tui.ui.query_request(&tui.snapshot).unwrap();
    let delayed = QueryResult {
        revisions: tui.snapshot.query_revisions(None).for_query(
            &old.query(),
            &tui.snapshot.library.playlist_entry_revisions,
            &tui.snapshot.library.metadata_revisions,
        ),
        view: ViewResponse::Track(Some(full_track(7))),
    };
    tui.ui.details.stamp = old_stamp;
    tui.ui.details.state = DetailState::Ready(Box::new(full_track(7)));
    let mut next = tui.snapshot.clone();
    next.library.bitrate_revision += 1;
    tui.apply_snapshot(next.clone());
    assert!(matches!(tui.ui.details.state, DetailState::Loading));
    assert_eq!((tui.ui.details.scroll, tui.ui.details.max_scroll), (4, 12));
    assert!(!tui.ui.apply_query(&next, &old, Ok(delayed)));
    let current = tui.ui.query_request(&next).unwrap();
    assert_ne!(current.stamp, old.stamp);
    let mut fresh_track = full_track(7);
    fresh_track.bitrate_bps = Some(1_536_000);
    fresh_track.bitrate_exact = true;
    assert!(tui.ui.apply_query(
        &next,
        &current,
        Ok(QueryResult {
            revisions: next.query_revisions(None).for_query(
                &current.query(),
                &next.library.playlist_entry_revisions,
                &next.library.metadata_revisions,
            ),
            view: ViewResponse::Track(Some(fresh_track)),
        })
    ));
    let DetailState::Ready(track) = &tui.ui.details.state else {
        unreachable!()
    };
    assert_eq!(
        (track.bitrate_bps, track.bitrate_exact),
        (Some(1_536_000), true)
    );
    assert_eq!((tui.ui.details.scroll, tui.ui.details.max_scroll), (4, 12));
}

#[test]
fn bitrate_changes_do_not_retire_playlist_location_lookups() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = playlist_view(1);
    let View::PlaylistDetail { detail, .. } = &mut tui.ui.view else {
        unreachable!()
    };
    detail.entry_anchor = Some(1);
    let pending = tui.ui.query_request(&state).unwrap();
    assert_eq!(
        pending.kind,
        QueryKind::PlaylistSelection {
            playlist_id: 1,
            entry_id: 1
        }
    );
    tui.ui.requested = Some(pending.clone());
    let mut next = state.clone();
    next.library.bitrate_revision += 1;
    tui.apply_snapshot(next.clone());
    assert_eq!(tui.ui.requested.as_ref(), Some(&pending));
    assert_eq!(tui.ui.query_request(&next), Some(pending.clone()));
    assert!(
        tui.ui
            .apply_query(&next, &pending, Ok(response(&pending, 12)),)
    );
    let page = tui.ui.query_request(&next).unwrap();
    assert_eq!(page.kind, QueryKind::Playlist(1));
    assert_eq!(
        page.stamp.revisions().bitrate,
        next.library.bitrate_revision
    );
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        unreachable!()
    };
    assert_eq!(detail.entries.selection.selected(), Some(1));
}

#[test]
fn external_playlist_rename_refreshes_active_and_cached_titles_without_reloading_entries() {
    for cached in [false, true] {
        let state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
        tui.ui.view = playlist_view(1);
        load_visible(&mut tui.ui, &state);
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
            panic!("playlist detail");
        };
        let coordinates = page_coordinates(&detail.entries);
        let selected = detail.entries.selected().unwrap() as *const PlaylistEntryRow;
        if cached {
            tui.ui.switch_tab(false, &state);
            load_visible(&mut tui.ui, &state);
        }

        let mut next = state.clone();
        next.library.playlist_revision += 1;
        tui.apply_snapshot(next.clone());
        if cached {
            let View::PlaylistDetail { detail, .. } = tui.ui.cached_playlists.as_ref().unwrap()
            else {
                panic!("cached playlist detail");
            };
            assert!(detail.summary_dirty);
            assert_eq!(detail.name, "collection");
            assert_eq!(page_coordinates(&detail.entries), coordinates);
            assert_eq!(detail.entries.selected().unwrap() as *const _, selected);
            // Hidden details are invalidated by snapshots, never polled.
            assert!(tui.ui.query_request(&next).is_none());
            tui.ui.switch_tab(true, &next);
        }
        let stale = tui.ui.query_request(&next).unwrap();
        assert_eq!(stale.kind, QueryKind::PlaylistSummary(1));
        assert_eq!(stale.query(), Query::PlaylistSummary { playlist_id: 1 });
        next.library.playlist_revision += 1;
        tui.apply_snapshot(next.clone());
        assert!(!tui.ui.apply_query(&next, &stale, Ok(response(&stale, 12))));
        let request = tui.ui.query_request(&next).unwrap();
        assert!(tui.ui.apply_query(
            &next,
            &request,
            Ok(QueryResult {
                revisions: next.query_revisions(None),
                view: ViewResponse::PlaylistSummary(Some(PlaylistSummary {
                    id: 1,
                    name: "Renamed 夜航".into(),
                    entry_count: 999,
                })),
            })
        ));
        let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
            panic!("playlist detail");
        };
        assert_eq!(detail.name, "Renamed 夜航");
        assert!(!detail.summary_dirty);
        assert_eq!(page_coordinates(&detail.entries), coordinates);
        assert_eq!(detail.entries.selected().unwrap() as *const _, selected);
        assert!(tui.ui.query_request(&next).is_none());
    }
}

#[test]
fn failed_playlist_entries_do_not_block_title_refresh_or_retry_after_it() {
    for prefetch in [false, true] {
        let state = query_snapshot();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
        tui.ui.view = playlist_view(1);
        if prefetch {
            let first = tui.ui.query_request(&state).unwrap();
            assert!(tui.ui.apply_query(&state, &first, Ok(response(&first, 12))));
        }
        let failed = tui.ui.query_request(&state).unwrap();
        assert_eq!(failed.offset, if prefetch { 3 } else { 0 });
        assert!(tui.ui.apply_query(
            &state,
            &failed,
            Err(QueryError::Message("read failed".into()))
        ));
        let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
            panic!("playlist detail");
        };
        let coordinates = page_coordinates(&detail.entries);
        let selected = detail.entries.selected().map(|row| row as *const _);

        let mut next = state.clone();
        next.library.playlist_revision += 1;
        tui.apply_snapshot(next.clone());
        let summary = tui
            .ui
            .query_request(&next)
            .expect("independent title refresh");
        assert_eq!(summary.kind, QueryKind::PlaylistSummary(1));
        assert!(tui.ui.apply_query(
            &next,
            &summary,
            Ok(QueryResult {
                revisions: next.query_revisions(None),
                view: ViewResponse::PlaylistSummary(Some(PlaylistSummary {
                    id: 1,
                    name: "Renamed".into(),
                    entry_count: 12,
                })),
            })
        ));
        let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
            panic!("playlist detail");
        };
        assert_eq!(detail.name, "Renamed");
        assert_eq!(page_coordinates(&detail.entries), coordinates);
        assert_eq!(
            detail.entries.selected().map(|row| row as *const _),
            selected
        );
        assert!(
            tui.ui.query_request(&next).is_none(),
            "failed entries stay suppressed"
        );
    }
}

#[test]
fn failed_playlist_title_does_not_block_pages_or_retry_when_they_load() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = playlist_view(1);
    load_visible(&mut tui.ui, &state);
    let mut next = state.clone();
    next.library.playlist_revision += 1;
    tui.apply_snapshot(next.clone());
    let summary = tui.ui.query_request(&next).unwrap();
    assert_eq!(summary.kind, QueryKind::PlaylistSummary(1));
    assert!(tui.ui.apply_query(
        &next,
        &summary,
        Err(QueryError::Message("summary failed".into()))
    ));
    for _ in 0..3 {
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &next);
    }
    let page = tui
        .ui
        .query_request(&next)
        .expect("independent page prefetch");
    assert_eq!(page.kind, QueryKind::Playlist(1));
    assert_eq!(page.offset, 6);
    assert!(tui.ui.apply_query(&next, &page, Ok(response(&page, 12))));
    assert!(
        tui.ui.query_request(&next).is_none(),
        "failed title stays suppressed"
    );

    next.library.playlist_revision += 1;
    tui.apply_snapshot(next.clone());
    let refreshed = tui.ui.query_request(&next).expect("new title revision");
    assert_eq!(refreshed.kind, QueryKind::PlaylistSummary(1));
    assert!(tui.ui.apply_query(
        &next,
        &refreshed,
        Err(QueryError::Message("summary still unavailable".into()))
    ));
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &next);
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &next);
    tui.ui.key_action(KeyEvent::from(KeyCode::Down), &next);
    let page = tui.ui.query_request(&next).unwrap();
    assert_eq!(page.kind, QueryKind::Playlist(1));
    assert!(tui.ui.apply_query(
        &next,
        &page,
        Err(QueryError::Message("entries unavailable".into()))
    ));
    assert!(
        tui.ui.query_request(&next).is_none(),
        "neither failure retries the other"
    );
}

#[test]
fn unexpected_query_payload_retires_demand_without_retrying_or_touching_newer_reads() {
    let state = query_snapshot();
    let mut ui = UiState {
        view: View::Playlists(PagedList::default()),
        ..UiState::default()
    };
    let request = ui.query_request(&state).unwrap();
    ui.requested = Some(request.clone());
    let malformed = || QueryResult {
        revisions: request.stamp.revisions(),
        view: ViewResponse::Track(None),
    };
    assert!(ui.apply_query(&state, &request, Ok(malformed())));
    assert!(
        ui.requested.is_none(),
        "completed malformed read is not in flight"
    );
    assert!(
        ui.query_request(&state).is_none(),
        "invalid payload is a terminal failure"
    );
    assert!(
        ui.message
            .as_ref()
            .is_some_and(|notice| notice.kind == NoticeKind::Error)
    );
    ui.switch_tab(false, &state);
    let current = ui.query_request(&state).unwrap();
    ui.requested = Some(current.clone());
    assert!(!ui.apply_query(&state, &request, Ok(malformed())));
    assert_eq!(ui.requested, Some(current));
}

#[test]
fn obsolete_query_errors_park_quietly_until_freshness_changes() {
    for error in [QueryError::RevisionChanged, QueryError::Cancelled] {
        for details in [false, true] {
            let mut state = query_snapshot();
            let mut ui = UiState {
                view: playlist_view(1),
                ..UiState::default()
            };
            if details {
                ui.details_active = true;
                ui.details.target = Some(1);
                ui.details.scroll = 7;
            }
            let request = ui.query_request(&state).unwrap();
            ui.requested = Some(request.clone());
            assert!(!ui.apply_query(&state, &request, Err(error.clone())));
            assert!(ui.requested.is_none());
            assert!(ui.message.is_none());
            assert!(
                ui.query_request(&state).is_none(),
                "same stale demand must not spin"
            );
            assert!(!matches!(ui.details.state, DetailState::Error(_)));
            state.library.revision += 1;
            if !details {
                *Arc::make_mut(&mut state.library.playlist_entry_revisions)
                    .get_mut(&1)
                    .unwrap() += 1;
            }
            let fresh = ui
                .query_request(&state)
                .expect("new freshness resumes demand");
            assert_ne!(fresh.stamp, request.stamp);
            ui.requested = Some(fresh.clone());
            assert!(!ui.apply_query(&state, &request, Err(error.clone())));
            assert_eq!(ui.requested, Some(fresh));
            if details {
                assert_eq!(ui.details.scroll, 7);
            }
        }
    }
}

#[test]
fn malformed_title_response_cannot_overwrite_entries_or_block_page_loading() {
    let state = query_snapshot();
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.view = playlist_view(1);
    load_visible(&mut tui.ui, &state);
    let mut next = state.clone();
    next.library.playlist_revision += 1;
    tui.apply_snapshot(next.clone());
    let request = tui.ui.query_request(&next).unwrap();
    assert_eq!(request.kind, QueryKind::PlaylistSummary(1));
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        panic!("playlist detail")
    };
    let coordinates = page_coordinates(&detail.entries);
    let wrong_page = QueryResult {
        revisions: rivu_protocol::response::QueryRevisions {
            playlist: u64::MAX,
            ..request.stamp.revisions()
        },
        view: ViewResponse::PlaylistEntries(Page {
            total: 0,
            rows: Vec::new(),
        }),
    };
    assert!(tui.ui.apply_query(&next, &request, Ok(wrong_page)));
    let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
        panic!("playlist detail")
    };
    assert_eq!(page_coordinates(&detail.entries), coordinates);
    for _ in 0..3 {
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &next);
    }
    let page = tui.ui.query_request(&next).unwrap();
    assert_eq!(page.kind, QueryKind::Playlist(1));
    assert!(tui.ui.apply_query(&next, &page, Ok(response(&page, 12))));
    assert!(tui.ui.query_request(&next).is_none());
}
