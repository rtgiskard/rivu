use super::*;
use crate::terminal::{
    state::{KeyAction, TuiState, tests::*},
    worker::{QueryKind, QueryResult},
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{Terminal, backend::TestBackend};
use rivu_protocol::{
    model::Query,
    response::{QueryRevisions, ViewResponse},
};
use std::sync::Arc;

#[test]
fn command_prompt_keeps_unicode_cursor_in_bounds() {
    for width in [1, 8, 40, 120] {
        let mut ui = UiState::new(Arc::new(Localizer::for_terminal("en-US")));
        ui.command_input = Some("界".repeat(80));
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
        terminal
            .draw(|frame| draw(frame, &TuiSnapshot::default(), &mut ui))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        assert!(cursor.x < width);
        assert!(cursor.y < 18);
    }
}

#[test]
fn recent_renders_filtered_source_indices() {
    let i18n = Localizer::for_terminal("en-US");
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let mut recent = RecentTracks {
        source: Arc::new(vec![
            rivu_protocol::model::HistoryEntry {
                track_id: 1,
                title: "Not selected".into(),
                play_count: 1,
                played_at: 0,
                favorite: false,
            },
            rivu_protocol::model::HistoryEntry {
                track_id: 2,
                title: "Recent title".into(),
                play_count: 12345,
                played_at: 1_700_000_000,
                favorite: true,
            },
        ]),
        playback: Some((2, 1_700_000_100)),
        ..RecentTracks::default()
    };
    recent.entries.insert(0, 256, 1, vec![1]);
    for width in [1, 2, 8, 24, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
        terminal
            .draw(|frame| draw_recent(frame, frame.area(), &mut recent, &i18n, colors))
            .unwrap();
        let text = terminal_text(&terminal);
        if width >= 24 {
            assert!(text.contains("Recent title"));
        }
        assert!(!text.contains("Not selected"));
    }
}

#[test]
fn long_detail_errors_scroll_independently_and_loading_preserves_ready_position() {
    let state = TuiSnapshot::default();
    for width in [2, 7, 40, 80] {
        let mut ui = UiState::new(Arc::new(Localizer::for_terminal("en-US")));
        ui.details_active = true;
        ui.details = Details {
            target: Some(2),
            source_label: "shell-panel-queue",
            state: DetailState::Ready(Box::new(full_track(2))),
            scroll: 4,
            ..Details::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        let ready_text = terminal_text(&terminal);
        let position = (ui.details.scroll, ui.details.max_scroll);
        assert_eq!(position.0, 4);

        ui.details.state = DetailState::Error(format!("{}\n终", "夜航e\u{301}👩‍🚀/\n".repeat(20)));
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert!(ui.details.error_max_scroll > ui.page_size);
        assert!(!terminal_text(&terminal).contains('终'));
        for (key, expected) in [
            (KeyCode::Down, 1),
            (KeyCode::Up, 0),
            (KeyCode::PageDown, ui.page_size),
            (KeyCode::PageUp, 0),
            (KeyCode::End, ui.details.error_max_scroll),
        ] {
            ui.key_action(KeyEvent::from(key), &state);
            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
            assert_eq!(ui.details.error_scroll, expected);
            assert_eq!((ui.details.scroll, ui.details.max_scroll), position);
        }
        assert!(terminal_text(&terminal).contains('终'));
        ui.key_action(KeyEvent::from(KeyCode::Home), &state);
        assert_eq!(ui.details.error_scroll, 0);

        ui.details.state = DetailState::Loading;
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        for key in [
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            ui.key_action(KeyEvent::from(key), &state);
            assert_eq!((ui.details.scroll, ui.details.max_scroll), position);
        }
        ui.details.state = DetailState::Ready(Box::new(full_track(2)));
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert_eq!((ui.details.scroll, ui.details.max_scroll), position);
        assert_eq!(terminal_text(&terminal), ready_text);
    }
}

#[test]
fn details_wrap_without_splitting_graphemes() {
    let text = "夜航e\u{301}👩‍🚀/".repeat(12);
    for width in [2, 7, 40] {
        let wrapped = wrap_details(&text, width);
        assert_eq!(wrapped.concat(), text);
        assert!(
            wrapped
                .iter()
                .all(|line| Span::raw(line.as_str()).width() <= width)
        );
        assert!(wrapped.iter().all(|line| !line.starts_with('\u{301}')));
    }
}

#[test]
fn help_hierarchy_wraps_and_scrolls_read_only_in_current_context() {
    for (language, width) in [("en-US", 96), ("zh-CN", 28)] {
        let mut ui = UiState::new(Arc::new(Localizer::for_terminal(language)));
        ui.show_help = true;
        let state = TuiSnapshot::default();
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert!(ui.help_max_scroll > 0);
        ui.key_action(KeyEvent::from(KeyCode::End), &state);
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert!(ui.help_scroll > 0);
        assert!(ui.show_help && !ui.details_active);
        ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
        assert!(!ui.show_help && !ui.details_active);
    }
}

#[test]
fn query_tail_preserves_graphemes_and_keeps_input_cursor_visible() {
    let query = "春风e\u{301}夜航";
    for language in ["en-US", "zh-CN"] {
        let mut search = SearchState {
            query: query.into(),
            editing: true,
            active: true,
            ..SearchState::default()
        };
        let i18n = Localizer::for_terminal(language);
        for width in [1, 8, 18, 24, 60] {
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
            terminal
                .draw(|frame| {
                    draw_search_input(
                        frame,
                        frame.area(),
                        &search,
                        &i18n,
                        TerminalColors::new(rivu_presentation::theme::Theme::default()),
                    )
                })
                .unwrap();
            let prefix = "󰍉 ";
            if width as usize > Span::raw(prefix).width() {
                assert!(terminal.backend().cursor_visible());
                let cursor = terminal.backend().cursor_position();
                assert!(cursor.x < width);
                assert_eq!(cursor.y, 0);
            }
        }
        search.editing = false;
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| {
                draw_search_input(
                    frame,
                    frame.area(),
                    &search,
                    &i18n,
                    TerminalColors::new(rivu_presentation::theme::Theme::default()),
                )
            })
            .unwrap();
        assert!(!terminal.backend().cursor_visible());
    }
    assert_eq!(query_tail(query, 5), ("e\u{301}夜航", 5));
    assert_eq!(query_tail(query, 1), ("", 0));
}

#[test]
fn bottom_search_preserves_viewport_and_input_cursor() {
    for width in [8, 40, 100] {
        let mut ui = UiState::default();
        let state = TuiSnapshot::default();
        let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        let page_size = ui.page_size;
        ui.search.active = true;
        ui.search.query = "夜航".into();
        for editing in [true, false] {
            ui.search.editing = editing;
            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
            assert_eq!(ui.page_size, page_size);
            assert_eq!(terminal.backend().cursor_visible(), editing);
            if editing {
                assert_eq!(terminal.backend().cursor_position().y, 15);
            }
        }
    }
}

#[test]
fn metadata_columns_clip_whole_graphemes_without_horizontal_overflow() {
    let title = "春风e\u{301}👩‍💻夜航".repeat(12);
    let mut track = track(0);
    track.title = title;
    track.duration = Some(243.0);
    track.favorite = true;
    track.missing = true;
    for width in [30, 60, 80, 110, 140] {
        let line = BrowserColumns::new(width, true, std::iter::once(RowContent::track(&track)))
            .line(
                RowContent::track(&track),
                TerminalColors::new(rivu_presentation::theme::Theme::default()),
            );
        assert_eq!(line.width(), width as usize);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!text.contains('\u{fffd}'));
    }
    assert_eq!(cell_text("春风", 3, false), "春…");
    assert_eq!(cell_text("e\u{301}夜航", 4, false), "e\u{301}夜…");
}

#[test]
fn tiny_browser_rows_preserve_title_without_horizontal_overflow() {
    let mut track = track(0);
    track.title = "tiny".into();
    track.artist = "artist ".repeat(20);
    track.album = "album ".repeat(20);
    track.codec = "PCM_S16LE".into();
    track.duration = Some(6003.0);
    for width in 0..=100 {
        let line = BrowserColumns::new(width, true, std::iter::once(RowContent::track(&track)))
            .line(
                RowContent::track(&track),
                TerminalColors::new(rivu_presentation::theme::Theme::default()),
            );
        assert_eq!(line.width(), width as usize);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        if width >= 9 {
            assert!(text.contains("tiny"), "width {width}: {text}");
        }
    }
}

#[test]
fn other_path_titles_expand_only_when_rendered_and_preserve_grapheme_clipping() {
    let mut track = track(0);
    track.title = "夜航e\u{301}👩‍🚀".into();
    track.path = "/音乐/春风/夜航.flac".into();
    let path = track.path.parent().unwrap();
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    for width in 0..=140 {
        let mut measured = RowContent::track(&track);
        measured.title_path = Some(path);
        measured.file_icon = true;
        assert!(matches!(measured.title, Cow::Borrowed(_)));
        let columns = BrowserColumns::new(width, true, std::iter::once(measured));
        let mut visible = RowContent::track(&track);
        visible.title_path = Some(path);
        visible.file_icon = true;
        let mut expanded = RowContent::track(&track);
        expanded.title = format!("{} · {}", track.title, path.display()).into();
        expanded.file_icon = true;
        let line = columns.line(visible, colors);
        assert_eq!(line.width(), width as usize);
        assert_eq!(line, columns.line(expanded, colors));
    }
}

#[test]
fn playback_markers_follow_occurrence_and_status_not_selection() {
    let mut state = TuiSnapshot::default();
    state.queue.current_id = Some(17);
    let mut page = PagedList::default();
    page.set_viewport(2);
    page.insert(
        0,
        2,
        2,
        vec![
            QueueRow {
                id: 17,
                track_id: 0,
                position: 0,
                offset: 0,
                track: track(0),
            },
            QueueRow {
                id: 18,
                track_id: 0,
                position: 1,
                offset: 1,
                track: track(0),
            },
        ],
    );
    page.selection.select(Some(1));
    for (status, symbol) in [
        (PlaybackStatus::Playing, ""),
        (PlaybackStatus::Paused, "󰏤"),
        (PlaybackStatus::Stopped, ""),
    ] {
        state.playback.status = status;
        let mut terminal = Terminal::new(TestBackend::new(80, 5)).unwrap();
        terminal
            .draw(|frame| {
                draw_queue(
                    frame,
                    frame.area(),
                    &state,
                    &mut page,
                    &Localizer::for_terminal("en-US"),
                    TerminalColors::new(state.system.ui_theme),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let marker_x = (0..buffer.area.width)
            .find(|&x| buffer[(x, 1)].symbol() == symbol)
            .expect("current queue occurrence has its playback marker");
        assert_eq!(buffer[(marker_x, 2)].symbol(), " ");
        assert_eq!(page.selected().unwrap().id, 18);
    }
}

#[test]
fn queue_mutations_relocate_selected_occurrence_and_clamp_deleted_selection() {
    let mut state = TuiSnapshot::default();
    state.queue.total = 12;
    state.system.query_batch_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
    tui.ui.queue.selection.select(Some(8));
    load_queue(&mut tui.ui, &state);
    state.queue.total = 11;
    state.queue.revision = 1;
    tui.apply_snapshot(state.clone());
    assert!(tui.ui.queue.pages.is_empty());
    let request = tui.ui.query_request(&state).unwrap();
    assert_eq!(request.demand.kind, QueryKind::QueueSelection(1008));
    assert!(
        matches!(request.demand.query(), Query::QueueEntries { queue_ids, query: None } if queue_ids == vec![1008])
    );
    let relocated = QueryResult {
        revisions: QueryRevisions {
            queue: 1,
            ..QueryRevisions::default()
        },
        view: ViewResponse::QueueEntries(vec![queue_row(2, 1008)]),
    };
    assert!(tui.ui.apply_query(&state, &request, Ok(relocated)));
    assert_eq!(tui.ui.queue.selection.selected(), Some(2));
    let request = tui.ui.query_request(&state).unwrap();
    assert_eq!(request.demand.offset, 0);
    let mut page = response(&request, state.queue.total);
    let ViewResponse::QueuePage(rows) = &mut page.view else {
        unreachable!()
    };
    rows.rows[2] = queue_row(2, 1008);
    tui.ui.apply_query(&state, &request, Ok(page));
    state.queue.total = 2;
    state.queue.revision = 2;
    tui.apply_snapshot(state.clone());
    let request = tui.ui.query_request(&state).unwrap();
    assert_eq!(request.demand.kind, QueryKind::QueueSelection(1008));
    tui.ui.apply_query(
        &state,
        &request,
        Ok(QueryResult {
            revisions: QueryRevisions {
                queue: 2,
                ..QueryRevisions::default()
            },
            view: ViewResponse::QueueEntries(Vec::new()),
        }),
    );
    assert_eq!(tui.ui.queue.selection.selected(), Some(1));
    load_queue(&mut tui.ui, &state);
    state.queue.total = 0;
    state.queue.revision = 3;
    tui.apply_snapshot(state.clone());
    assert_eq!(tui.ui.queue.selection.selected(), None);
    assert!(tui.ui.query_request(&state).is_none());
    assert!(matches!(
        tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
        KeyAction::Ignored
    ));
    let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
    terminal
        .draw(|frame| {
            draw_queue(
                frame,
                frame.area(),
                &state,
                &mut tui.ui.queue,
                &tui.ui.i18n,
                TerminalColors::new(state.system.ui_theme),
            )
        })
        .unwrap();
}

pub(in crate::terminal) fn render_view(ui: &mut UiState) -> String {
    let mut terminal = Terminal::new(TestBackend::new(60, 5)).unwrap();
    terminal
        .draw(|frame| {
            draw_current_page(
                frame,
                frame.area(),
                &TuiSnapshot::default(),
                ui,
                TerminalColors::new(rivu_presentation::theme::Theme::default()),
            );
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
