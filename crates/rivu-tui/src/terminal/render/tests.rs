use super::*;
use crate::terminal::{
    state::{KeyAction, TuiState, tests::*},
    worker::{QueryResult, ReadKind},
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
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &TuiSnapshot::default(), &mut ui)
            })
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
            .draw(|frame| {
                recent
                    .entries
                    .set_viewport(frame.area().height.saturating_sub(1) as usize);
                draw_recent(frame, frame.area(), &mut recent, &i18n, colors)
            })
            .unwrap();
        let text = terminal_text(&terminal);
        if width >= 24 {
            assert!(text.contains("Recent title"));
        }
        assert!(!text.contains("Not selected"));
    }
}

#[test]
fn recent_refreshes_count_and_live_timestamp_without_losing_selection() {
    let i18n = Localizer::for_terminal("en-US");
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let mut recent = RecentTracks {
        source: Arc::new(vec![rivu_protocol::model::HistoryEntry {
            track_id: 2,
            title: "夜航e\u{301}👩‍🚀".repeat(20),
            play_count: 12345,
            played_at: 1_700_000_000,
            favorite: false,
        }]),
        ..RecentTracks::default()
    };
    recent.entries.insert(0, 256, 1, vec![0]);
    for width in [24, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
        for (count, timestamp, old_count) in
            [(12345, 1_700_000_100, "987"), (987, 1_700_000_200, "12345")]
        {
            Arc::make_mut(&mut recent.source)[0].play_count = count;
            recent.playback = Some((2, timestamp));
            terminal
                .draw(|frame| draw_recent(frame, frame.area(), &mut recent, &i18n, colors))
                .unwrap();
            let text = terminal_text(&terminal);
            assert!(text.contains(&count.to_string()), "{text}");
            assert!(!text.contains(old_count), "{text}");
            if width == 80 {
                let time = chrono::DateTime::from_timestamp(timestamp, 0)
                    .unwrap()
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string();
                assert!(text.contains(&time), "{text}");
            }
            assert_eq!(recent.entries.selected(), Some(&0));
        }
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
            state: DetailState::Ready(Arc::new(full_track(2))),
            scroll: 4,
            ..Details::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui)
            })
            .unwrap();
        let ready_text = terminal_text(&terminal);
        let position = (ui.details.scroll, ui.details.max_scroll);
        assert_eq!(position.0, 4);

        ui.details.state =
            DetailState::Error(format!("{}\n终", "夜航e\u{301}👩‍🚀/\n".repeat(20)).into());
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui)
            })
            .unwrap();
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
            terminal
                .draw(|frame| {
                    commit_layout(frame.area(), &mut ui);
                    draw(frame, &state, &mut ui)
                })
                .unwrap();
            assert_eq!(ui.details.error_scroll, expected);
            assert_eq!((ui.details.scroll, ui.details.max_scroll), position);
        }
        assert!(terminal_text(&terminal).contains('终'));
        ui.key_action(KeyEvent::from(KeyCode::Home), &state);
        assert_eq!(ui.details.error_scroll, 0);

        ui.details.state = DetailState::Loading;
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui)
            })
            .unwrap();
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
        ui.details.state = DetailState::Ready(Arc::new(full_track(2)));
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui)
            })
            .unwrap();
        assert_eq!((ui.details.scroll, ui.details.max_scroll), position);
        assert_eq!(terminal_text(&terminal), ready_text);
    }
}

#[test]
fn iteration_detail_fields_keep_label_and_value_visible_at_widths_one_through_four() {
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    for width in 1..=4 {
        let lines = detail_field("Artist", "Visible", width, colors);
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 32)).unwrap();
        terminal
            .draw(|frame| draw_cached_lines(frame, frame.area(), &lines, 0))
            .unwrap();
        let rendered = terminal_text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        assert_eq!(
            rendered, "ArtistVisible",
            "field text lost at width {width}"
        );
        assert!(lines.iter().all(|line| line.width() <= width));
    }
}

#[test]
fn iteration_details_refresh_replaced_error_and_container_text() {
    for container in [false, true] {
        let mut ui = UiState::new(Arc::new(Localizer::for_terminal("en-US")));
        ui.details_active = true;
        ui.details.target = (!container).then_some(7);
        ui.details.source_label = "shell-panel-queue";
        ui.details.state = if container {
            DetailState::Container("Original message".into())
        } else {
            DetailState::Error("Original message".into())
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &TuiSnapshot::default(), &mut ui)
            })
            .unwrap();
        assert!(terminal_text(&terminal).contains("Original message"));
        ui.details.state = if container {
            DetailState::Container("Updated message".into())
        } else {
            DetailState::Error("Updated message".into())
        };
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &TuiSnapshot::default(), &mut ui)
            })
            .unwrap();
        let text = terminal_text(&terminal);
        assert!(text.contains("Updated message"));
        assert!(!text.contains("Original message"));
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
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui)
            })
            .unwrap();
        assert!(ui.help_max_scroll > 0);
        ui.key_action(KeyEvent::from(KeyCode::End), &state);
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui)
            })
            .unwrap();
        assert!(ui.help_scroll > 0);
        assert!(ui.show_help && !ui.details_active);
        ui.key_action(KeyEvent::from(KeyCode::Esc), &state);
        assert!(!ui.show_help && !ui.details_active);
    }
}

#[test]
fn cached_text_preserves_nonwrapping_unicode_clipping_and_style() {
    let lines = vec![
        Line::from(vec![
            Span::styled("界", Style::default().fg(ratatui::style::Color::Red)),
            Span::raw("a"),
        ]),
        Line::from("加载中…"),
        Line::from("e\u{301}👩‍🚀z"),
    ];
    for width in [1, 2, 3, 7] {
        for scroll in 0..lines.len() {
            let mut cached = Terminal::new(TestBackend::new(width, 3)).unwrap();
            cached
                .draw(|frame| draw_cached_lines(frame, frame.area(), &lines, scroll))
                .unwrap();
            let mut expected = Terminal::new(TestBackend::new(width, 3)).unwrap();
            expected
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(lines.clone()).scroll((scroll as u16, 0)),
                        frame.area(),
                    );
                })
                .unwrap();
            assert_eq!(cached.backend().buffer(), expected.backend().buffer());
        }
    }
}

#[test]
fn details_refresh_after_metadata_changes_without_reopening() {
    let state = TuiSnapshot::default();
    let mut track = full_track(2);
    track.title = "Original title".into();
    let mut ui = UiState::new(Arc::new(Localizer::for_terminal("en-US")));
    ui.details_active = true;
    ui.details = Details {
        target: Some(2),
        source_label: "shell-panel-queue",
        state: DetailState::Ready(Arc::new(track)),
        ..Details::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            commit_layout(frame.area(), &mut ui);
            draw(frame, &state, &mut ui)
        })
        .unwrap();
    assert!(terminal_text(&terminal).contains("Original title"));
    let DetailState::Ready(track) = &mut ui.details.state else {
        unreachable!()
    };
    Arc::make_mut(track).title = "Updated title".into();
    terminal
        .draw(|frame| {
            commit_layout(frame.area(), &mut ui);
            draw(frame, &state, &mut ui)
        })
        .unwrap();
    let text = terminal_text(&terminal);
    assert!(text.contains("Updated title"));
    assert!(!text.contains("Original title"));
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
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui)
            })
            .unwrap();
        let page_size = ui.page_size;
        ui.search.active = true;
        ui.search.query = "夜航".into();
        for editing in [true, false] {
            ui.search.editing = editing;
            terminal
                .draw(|frame| {
                    commit_layout(frame.area(), &mut ui);
                    draw(frame, &state, &mut ui)
                })
                .unwrap();
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
    let intrinsic = IntrinsicColumns::measure(
        BrowserMode::Tracks,
        std::iter::once(RowContent::track(&track)),
    );
    for width in [30, 60, 80, 110, 140] {
        let line = BrowserColumns::fit(width, intrinsic).line(
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
    let intrinsic = IntrinsicColumns::measure(
        BrowserMode::Tracks,
        std::iter::once(RowContent::track(&track)),
    );
    for width in 0..=100 {
        let line = BrowserColumns::fit(width, intrinsic).line(
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
        let columns = BrowserColumns::fit(
            width,
            IntrinsicColumns::measure(BrowserMode::Tracks, std::iter::once(measured)),
        );
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
                page.set_viewport(frame.area().height.saturating_sub(1) as usize);
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
        let colors = TerminalColors::new(state.system.ui_theme);
        let mut markers = buffer
            .content
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.symbol() == symbol);
        let (marker_index, marker) = markers
            .next()
            .expect("current occurrence has its playback marker");
        assert!(
            markers.next().is_none(),
            "only the current occurrence is marked"
        );
        assert_ne!(
            marker.bg, colors.selected,
            "selection alone must not move the playback marker"
        );
        let marker_x = marker_index % buffer.area.width as usize;
        let selected_row = buffer
            .content
            .chunks(buffer.area.width as usize)
            .find(|row| row.iter().any(|cell| cell.bg == colors.selected))
            .expect("selected occurrence is visible");
        assert_eq!(selected_row[marker_x].symbol(), " ");
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
    assert_eq!(request.demand.kind(), ReadKind::QueueSelection(1008));
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
    assert_eq!(request.demand.kind(), ReadKind::QueueSelection(1008));
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
            tui.ui
                .queue
                .set_viewport(frame.area().height.saturating_sub(1) as usize);
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
            let rows = frame
                .area()
                .height
                .saturating_sub(page_heading_rows(&ui.view))
                .max(1);
            commit_page_viewport(ui, rows as usize);
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

#[test]
fn retained_rows_size_columns_until_replaced_trimmed_or_cleared_and_resize_refits_them() {
    let i18n = Localizer::for_terminal("en-US");
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let render = |page: &mut PagedList<LibraryRow>, width, mode| {
        let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
        terminal
            .draw(|frame| {
                draw_paged_list(
                    frame,
                    frame.area(),
                    "Rows".into(),
                    page,
                    |track| RowContent::track(track),
                    &i18n,
                    BrowserStyle { mode, colors },
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    };
    let mut short = track(0);
    short.title = "Title".into();
    short.artist = "a".into();
    short.album.clear();
    let mut wide = short.clone();
    wide.artist = "艺".repeat(8);
    let mut page = PagedList::default();
    page.set_viewport(2);
    page.insert(0, 2, 10, vec![short.clone(), short.clone()]);
    // Neighboring retained rows, not only visible rows, set intrinsic widths.
    page.insert(2, 2, 10, vec![wide.clone()]);
    assert_eq!(
        render(&mut page, 100, BrowserMode::Tracks)[(64, 1)].symbol(),
        "a"
    );
    assert_eq!(
        render(&mut page, 100, BrowserMode::Tracks)[(64, 1)].symbol(),
        "a"
    );
    assert_eq!(
        render(&mut page, 110, BrowserMode::Tracks)[(74, 1)].symbol(),
        "a"
    );

    page.insert(2, 2, 10, vec![short.clone()]);
    assert_eq!(
        render(&mut page, 100, BrowserMode::Tracks)[(79, 1)].symbol(),
        "a"
    );
    page.insert(2, 2, 10, vec![wide.clone()]);
    assert_eq!(
        render(&mut page, 100, BrowserMode::Tracks)[(64, 1)].symbol(),
        "a"
    );
    page.selection.select(Some(8));
    *page.selection.offset_mut() = 8;
    page.set_viewport(2);
    page.insert(8, 2, 10, vec![short.clone(), short.clone()]);
    assert_eq!(
        render(&mut page, 100, BrowserMode::Tracks)[(79, 1)].symbol(),
        "a"
    );
    assert!(!page.pages.contains_key(&2));

    // Payload-only metadata/bitrate refreshes clear pages without changing the
    // saved navigation coordinates; their replacement must resize columns too.
    page.clear_pages();
    page.insert(8, 2, 10, vec![short.clone(), wide]);
    assert_eq!(
        render(&mut page, 100, BrowserMode::Tracks)[(64, 1)].symbol(),
        "a"
    );
    page.selection.select(Some(0));
    page.insert(0, 3, 3, vec![short.clone(), short.clone(), short]);
    assert_eq!(
        render(&mut page, 100, BrowserMode::Tracks)[(79, 1)].symbol(),
        "a"
    );
    let labels = render(&mut page, 100, BrowserMode::Labels);
    assert!(!labels.content.iter().any(|cell| cell.symbol() == "f"));
    let tracks = render(&mut page, 100, BrowserMode::Tracks);
    assert!(tracks.content.iter().any(|cell| cell.symbol() == "f"));
    assert_eq!(tracks[(79, 1)].symbol(), "a");
}

#[test]
fn drawing_without_a_layout_commit_cannot_change_retention_or_navigation() {
    let mut ui = UiState::default();
    ui.queue.viewport_rows = 2;
    ui.page_size = 2;
    ui.queue.total = 100;
    ui.queue.loaded = true;
    ui.queue.fetch_size = 1;
    ui.queue.pages.insert(0, vec![queue_row(0, 1000)]);
    ui.queue.pages.insert(80, vec![queue_row(80, 1080)]);
    let mut retained = ui.queue.pages.keys().copied().collect::<Vec<_>>();
    retained.sort_unstable();
    let coordinates = (
        ui.queue.selection.selected(),
        ui.queue.selection.offset(),
        ui.queue.viewport_rows,
        ui.page_size,
    );
    for height in [3, 24, 5] {
        let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, &TuiSnapshot::default(), &mut ui))
            .unwrap();
        let mut after = ui.queue.pages.keys().copied().collect::<Vec<_>>();
        after.sort_unstable();
        assert_eq!(after, retained);
        assert_eq!(
            (
                ui.queue.selection.selected(),
                ui.queue.selection.offset(),
                ui.queue.viewport_rows,
                ui.page_size
            ),
            coordinates
        );
    }
    commit_layout(Rect::new(0, 0, 80, 24), &mut ui);
    assert!(!ui.queue.pages.contains_key(&80));
    assert_ne!(ui.queue.viewport_rows, coordinates.2);
}

#[test]
fn missing_titles_use_shared_fallback_in_lists_playback_and_details() {
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let i18n = Arc::new(Localizer::for_terminal("en-US"));
    for title in ["", " \t\n", "\u{2003}", "  Valid title  "] {
        for cue_number in [None, Some(3)] {
            let expected = if !title.trim().is_empty() {
                title
            } else if cue_number.is_some() {
                "night.live · #03"
            } else {
                "night.live"
            };
            let mut row = track(2);
            row.path = "/music/night.live.flac".into();
            row.title = title.into();
            row.cue_number = cue_number;
            let columns = BrowserColumns {
                title: 80,
                artist: 0,
                album: 0,
                codec: 0,
                duration: 0,
                markers: 0,
            };
            let rendered = columns.line(RowContent::track(&row), colors).to_string();
            assert!(rendered.contains(&format!("{expected} — {}", row.artist)));
            let entry = PlaylistEntryRow {
                id: 7,
                track_id: row.id,
                position: 0,
                offset: 0,
                path: row.path.clone(),
                title: row.title.clone(),
                artist: row.artist.clone(),
                album: row.album.clone(),
                duration: row.duration,
                codec: row.codec.clone(),
                channels: row.channels,
                sample_rate: row.sample_rate,
                bits_per_sample: row.bits_per_sample,
                bitrate_bps: row.bitrate_bps,
                bitrate_exact: row.bitrate_exact,
                cue_number,
                favorite: false,
                missing: false,
            };
            assert_eq!(
                columns
                    .line(RowContent::playlist(&entry), colors)
                    .to_string(),
                rendered
            );

            let mut current = full_track(2);
            current.path = row.path;
            current.title = title.into();
            current.artist = "artist".into();
            current.cue = cue_number.map(|number| rivu_protocol::model::CueSegment {
                sheet: "/music/night.cue".into(),
                number,
                start_frame: 0,
                end_frame: None,
            });
            let current = Arc::new(current);
            let snapshot = TuiSnapshot {
                current_track: Some(Arc::clone(&current)),
                ..Default::default()
            };
            let mut ui = UiState::new(Arc::clone(&i18n));
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal
                .draw(|frame| {
                    commit_layout(frame.area(), &mut ui);
                    draw(frame, &snapshot, &mut ui);
                })
                .unwrap();
            assert!(terminal_text(&terminal).contains(&format!("{expected} — artist")));
            let details = Details {
                target: Some(2),
                source_label: "shell-panel-queue",
                state: DetailState::Ready(current),
                ..Details::default()
            };
            assert_eq!(
                details_lines(&details, &i18n, 100, colors)[0].to_string(),
                expected
            );
        }
    }
}
