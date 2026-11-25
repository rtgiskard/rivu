use super::*;
use crate::terminal::state::{TuiState, tests::*};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{Terminal, backend::TestBackend};
use rivu_protocol::{model::Query, response::ViewResponse};
use std::sync::Arc;

#[test]
fn progress_changes_at_seconds_or_visible_cell_boundaries() {
    let mut state = TuiSnapshot::default();
    state.playback.duration = Some(180.0);
    let progress = |width, position| {
        let mut state = state.clone();
        state.playback.position = position;
        PlaybackProgress::new(width, &state)
    };
    assert!(progress(40, 2.0) == progress(40, 2.3));
    assert!(progress(40, 2.3) != progress(40, 3.0));
    assert!(progress(180, 2.0) != progress(180, 2.3));
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    for (width, duration) in [(40, 180.), (180, 180.), (60, 12_345.)] {
        state.playback.duration = Some(duration);
        let mut previous = None;
        for position in [2., 2.3, 3., 60., duration / 2.] {
            state.playback.position = position;
            let key = PlaybackProgress::new(width, &state);
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| key.draw(frame, frame.area(), colors))
                .unwrap();
            let painted = terminal.backend().buffer().clone();
            if let Some((old_key, old_paint)) = previous {
                assert_eq!(key == old_key, painted == old_paint);
            }
            previous = Some((key, painted));
        }
    }
}

#[test]
fn queue_replacement_confirmation_uses_status_row_without_hiding_notices() {
    let mut tui = queue_replace_tui();
    tui.ui
        .set_notice("retained error".into(), NoticeKind::Error);
    tui.ui
        .key_action(KeyEvent::from(KeyCode::Enter), &tui.snapshot);
    tui.ui.queue_replace.as_mut().unwrap().prompt = "pending collection intent".into();
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let mut terminal = Terminal::new(TestBackend::new(80, 2)).unwrap();
    terminal
        .draw(|frame| {
            draw_status(
                frame,
                Rect::new(0, 0, 80, 1),
                &tui.snapshot,
                &tui.ui,
                colors,
            );
            draw_messages(
                frame,
                Rect::new(0, 1, 80, 1),
                &tui.ui,
                &tui.snapshot,
                colors,
            );
        })
        .unwrap();
    let rendered = terminal_text(&terminal);
    assert!(rendered.contains("pending collection intent"));
    assert!(rendered.contains("retained error"));
    tui.ui
        .key_action(KeyEvent::from(KeyCode::Esc), &tui.snapshot);
    terminal
        .draw(|frame| {
            draw_status(frame, frame.area(), &tui.snapshot, &tui.ui, colors);
        })
        .unwrap();
    assert!(!terminal_text(&terminal).contains("pending collection intent"));
}

#[test]
fn status_row_shows_speed_only_when_not_normal() {
    let ui = UiState::new(Arc::new(Localizer::for_terminal("en-US")));
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let mut state = TuiSnapshot::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
    for (speed, expected) in [(1.0, None), (1.25, Some("1.25×")), (0.5, Some("0.50×"))] {
        state.playback.speed = speed;
        terminal
            .draw(|frame| draw_status(frame, frame.area(), &state, &ui, colors))
            .unwrap();
        let rendered = terminal_text(&terminal);
        match expected {
            Some(text) => {
                assert!(rendered.contains(text), "{rendered}");
                let buffer = terminal.backend().buffer();
                let marker = (0..buffer.area.width)
                    .map(|x| &buffer[(x, 0)])
                    .find(|cell| cell.symbol() == "×")
                    .expect("speed marker is visible");
                assert_eq!(marker.fg, colors.accent);
            }
            None => assert!(!rendered.contains('×'), "{rendered}"),
        }
    }
}

#[test]
fn current_error_overrides_retained_scan_but_not_active_scan_or_notice() {
    use rivu_protocol::model::{ScanPhase, ScanProgress};

    let mut ui = UiState::new(Arc::new(Localizer::for_terminal("en-US")));
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let mut state = TuiSnapshot::default();
    state.system.scan_progress = Some(ScanProgress {
        phase: ScanPhase::Completed,
        processed: 1,
        total: Some(1),
        errors: 0,
        path: None,
    });
    state.system.scan_message = "retained scan result".into();
    let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
    for (scanning, error, notice, expected) in [
        (false, None, None, "retained scan result"),
        (false, Some("playback failed"), None, "playback failed"),
        (true, Some("playback failed"), None, "retained scan result"),
        (
            true,
            Some("playback failed"),
            Some("current notice"),
            "current notice",
        ),
    ] {
        state.system.scanning = scanning;
        state.system.last_error = error.map(str::to_owned);
        ui.message = notice.map(|text| crate::terminal::state::Notice {
            kind: NoticeKind::Error,
            text: text.into(),
            expires_at: std::time::Instant::now(),
        });
        terminal
            .draw(|frame| draw_messages(frame, frame.area(), &ui, &state, colors))
            .unwrap();
        assert!(terminal_text(&terminal).contains(expected));
    }
}

#[test]
fn command_prompt_keeps_unicode_cursor_in_bounds() {
    let long_input = "界e\u{301}".repeat(80);
    for (input, width, visible) in [
        ("", 8, true),
        ("a界e\u{301}", 8, true),
        (long_input.as_str(), 1, false),
        (long_input.as_str(), 2, false),
        (long_input.as_str(), 3, false),
        (long_input.as_str(), 4, true),
        (long_input.as_str(), 8, true),
        (long_input.as_str(), 40, true),
        (long_input.as_str(), 120, true),
    ] {
        let mut ui = UiState::new(Arc::new(Localizer::for_terminal("en-US")));
        ui.command_input = Some(input.to_owned());
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &TuiSnapshot::default(), &mut ui);
            })
            .unwrap();
        assert_eq!(
            terminal.backend().cursor_visible(),
            visible,
            "width={width}, input={input:?}"
        );
        if visible {
            let backend = terminal.backend();
            let cursor = backend.cursor_position();
            assert_eq!(cursor.y, backend.buffer().area.bottom() - 1);
            let buffer = backend.buffer();
            let prompt = (0..width)
                .find(|&x| buffer[(x, cursor.y)].symbol() == ":")
                .unwrap();
            let painted = (prompt + 1..width)
                .map(|x| buffer[(x, cursor.y)].symbol())
                .filter(|symbol| *symbol != " ")
                .collect::<String>();
            assert!(input.ends_with(&painted));
            if width >= 8 && !input.is_empty() {
                assert!(!painted.is_empty());
            }
            assert_eq!(cursor.x, prompt + 1 + Span::raw(painted).width() as u16);
            assert!(cursor.x < width);
        }
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
                draw(frame, &state, &mut ui);
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
                draw(frame, &state, &mut ui);
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
                    draw(frame, &state, &mut ui);
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
                draw(frame, &state, &mut ui);
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
                draw(frame, &state, &mut ui);
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
                draw(frame, &TuiSnapshot::default(), &mut ui);
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
                draw(frame, &TuiSnapshot::default(), &mut ui);
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
        assert!(
            wrapped
                .iter()
                .flat_map(|line| line.graphemes(true))
                .eq(text.graphemes(true))
        );
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
                draw(frame, &state, &mut ui);
            })
            .unwrap();
        assert!(ui.help_max_scroll > 0);
        ui.key_action(KeyEvent::from(KeyCode::End), &state);
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui);
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
            draw(frame, &state, &mut ui);
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
            draw(frame, &state, &mut ui);
        })
        .unwrap();
    let text = terminal_text(&terminal);
    assert!(text.contains("Updated title"));
    assert!(!text.contains("Original title"));
}

#[test]
fn retained_detail_layout_survives_refresh_until_replacement_or_expiry() {
    for expire in [false, true] {
        let state = query_snapshot();
        let query = Query::Track { track_id: 2 };
        let revisions = state.query_revisions(None).for_query(
            &query,
            &state.library.playlist_entry_revisions,
            &state.library.metadata_revisions,
        );
        let mut track = full_track(2);
        track.title = "Original title".into();
        let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::for_terminal("en-US")));
        tui.ui.details_active = true;
        tui.ui.details = Details {
            target: Some(2),
            source_label: "shell-panel-queue",
            state: DetailState::Ready(Arc::new(track)),
            stamp: Some(query.stamp(revisions)),
            scroll: 4,
            ..Details::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &tui.snapshot, &mut tui.ui);
            })
            .unwrap();
        let ready_text = terminal_text(&terminal);
        let position = (tui.ui.details.scroll, tui.ui.details.max_scroll);
        let mut next = state;
        next.library.bitrate_revision += 1;
        tui.apply_snapshot(next);
        let deadline = tui.ui.details.refresh_deadline().unwrap();
        let request = tui.ui.query_request(&tui.snapshot).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &tui.snapshot, &mut tui.ui);
            })
            .unwrap();
        assert_eq!(terminal_text(&terminal), ready_text);
        assert_eq!((tui.ui.details.scroll, tui.ui.details.max_scroll), position);
        tui.ui
            .key_action(KeyEvent::from(KeyCode::Home), &tui.snapshot);
        assert_eq!(tui.ui.details.scroll, 0);
        terminal
            .draw(|frame| {
                draw(frame, &tui.snapshot, &mut tui.ui);
            })
            .unwrap();
        assert!(terminal_text(&terminal).contains("Original title"));
        if expire {
            assert!(tui.ui.details.expire_refresh(deadline));
        } else {
            let mut track = full_track(2);
            track.title = "Replacement title".into();
            assert!(tui.ui.apply_query(
                &tui.snapshot,
                &request,
                Ok(result(&request, ViewResponse::Track(Some(track)))),
            ));
        }
        terminal
            .draw(|frame| {
                draw(frame, &tui.snapshot, &mut tui.ui);
            })
            .unwrap();
        assert!(!terminal_text(&terminal).contains("Original title"));
        assert!(tui.ui.details.refresh_deadline().is_none());
        if expire {
            assert!(
                terminal_text(&terminal).contains(tui.ui.i18n.text("terminal-details-loading"))
            );
        } else {
            assert!(terminal_text(&terminal).contains("Replacement title"));
        }
    }
}

#[test]
fn query_tail_preserves_graphemes() {
    let query = "春风e\u{301}夜航";
    assert_eq!(query_tail(query, 5), ("e\u{301}夜航", 5));
    assert_eq!(query_tail(query, 1), ("", 0));
}

#[test]
fn bottom_search_preserves_viewport_and_input_cursor() {
    for (width, room_for_cursor) in [
        (3, false),
        (8, true),
        (18, true),
        (24, true),
        (40, true),
        (60, true),
        (100, true),
    ] {
        let mut ui = UiState::default();
        let state = TuiSnapshot::default();
        let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
        terminal
            .draw(|frame| {
                commit_layout(frame.area(), &mut ui);
                draw(frame, &state, &mut ui);
            })
            .unwrap();
        let page_size = ui.page_size;
        ui.search.active = true;
        ui.search.query = "春风e\u{301}夜航".into();
        for editing in [true, false] {
            ui.search.editing = editing;
            terminal
                .draw(|frame| {
                    commit_layout(frame.area(), &mut ui);
                    draw(frame, &state, &mut ui);
                })
                .unwrap();
            assert_eq!(ui.page_size, page_size);
            let visible = editing && room_for_cursor;
            assert_eq!(
                terminal.backend().cursor_visible(),
                visible,
                "width={width}"
            );
            if visible {
                let cursor = terminal.backend().cursor_position();
                assert!(cursor.x < width);
                assert_eq!(cursor.y, 15);
            }
        }
    }
}

#[test]
fn metadata_columns_clip_whole_graphemes_without_horizontal_overflow() {
    let title = "春风e\u{301}👩‍💻夜航".repeat(40);
    let mut track = track(0);
    track.title = title.clone();
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
        let start = text.find('春').expect("title prefix");
        let (prefix, _) = text[start..].split_once('…').expect("clipped title");
        assert!(title.starts_with(prefix));
        assert!(
            title
                .grapheme_indices(true)
                .any(|(end, _)| end == prefix.len())
        );
    }
    assert_eq!(cell_text("春风", 3, false), "春…");
    assert_eq!(cell_text("e\u{301}夜航", 4, false), "e\u{301}夜…");
    // Hidden characters must not split a grapheme that becomes wider once joined.
    assert_eq!(cell_text("#\n\u{FE0F}x", 1, false), "…");
    assert_eq!(cell_text("#\n\u{FE0F}x", 3, false), "#\u{FE0F}x");
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
                position: 0,
                offset: 0,
                track: track(0),
            },
            QueueRow {
                id: 18,
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
                    TerminalColors::new(state.configuration.ui_theme),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let colors = TerminalColors::new(state.configuration.ui_theme);
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
        let buffer = terminal.backend().buffer();
        (0..width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>()
    };
    let mut short = track(0);
    short.title = "Title".into();
    short.artist = "Zed".into();
    short.album.clear();
    let mut wide = short.clone();
    wide.artist = "艺".repeat(8);
    let mut page = PagedList::default();
    page.set_viewport(2);
    page.insert(0, 2, 10, vec![short.clone(), short.clone()]);
    let artist = |page: &mut PagedList<LibraryRow>, width| {
        render(page, width, BrowserMode::Tracks)
            .find("Zed")
            .expect("visible artist")
    };
    let narrow = artist(&mut page, 100);
    // An offscreen retained neighbor affects the visible row's artist position.
    page.insert(2, 2, 10, vec![wide.clone()]);
    let wide_position = artist(&mut page, 100);
    assert!(wide_position < narrow);
    assert_eq!(artist(&mut page, 100), wide_position);
    assert!(artist(&mut page, 110) > wide_position);
    page.insert(2, 2, 10, vec![short.clone()]);
    assert_eq!(artist(&mut page, 100), narrow);
    page.insert(2, 2, 10, vec![wide.clone()]);
    assert_eq!(artist(&mut page, 100), wide_position);
    page.selection.select(Some(8));
    *page.selection.offset_mut() = 8;
    page.set_viewport(2);
    page.insert(8, 2, 10, vec![short.clone(), short.clone()]);
    assert_eq!(artist(&mut page, 100), narrow);
    assert!(!page.pages.contains_key(&2));

    page.clear_pages();
    page.insert(8, 2, 10, vec![short.clone(), wide]);
    assert_eq!(artist(&mut page, 100), wide_position);
    page.selection.select(Some(0));
    page.insert(0, 3, 3, vec![short.clone(), short.clone(), short]);
    assert_eq!(artist(&mut page, 100), narrow);
    assert!(!render(&mut page, 100, BrowserMode::Labels).contains("flac"));
    assert!(render(&mut page, 100, BrowserMode::Tracks).contains("flac"));
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
            .draw(|frame| {
                draw(frame, &TuiSnapshot::default(), &mut ui);
            })
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
fn smaller_draw_frame_reflows_the_selection_after_a_larger_layout_commit() {
    let mut ui = UiState::default();
    let state = TuiSnapshot::default();
    commit_layout(Rect::new(0, 0, 80, 24), &mut ui);
    ui.queue
        .insert(0, 32, 32, (0..32).map(|n| queue_row(n, n as u64)).collect());
    ui.queue.selection.select(Some(12));
    assert_eq!(ui.queue.selection.offset(), 0);

    let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
    let body = page_layout(Rect::new(0, 0, 80, 14))[1];
    terminal
        .draw(|frame| {
            commit_layout(frame.area(), &mut ui);
            draw(frame, &state, &mut ui);
        })
        .unwrap();

    assert_eq!(ui.page_size, usize::from(body.height));
    assert_eq!(ui.queue.selection.offset(), 13 - ui.page_size);
    let selected_bg = TerminalColors::new(state.configuration.ui_theme).selected;
    let buffer = terminal.backend().buffer();
    assert!((body.x..body.right()).any(|x| buffer[(x, body.bottom() - 1)].bg == selected_bg));
}

#[test]
fn missing_titles_use_shared_fallback_in_lists_playback_and_details() {
    let colors = TerminalColors::new(rivu_presentation::theme::Theme::default());
    let i18n = Arc::new(Localizer::for_terminal("en-US"));
    let title = "";
    let cue_number = Some(3);
    let expected = "night.live · #03";
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
    assert!(rendered.contains(expected));
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
    assert!(
        columns
            .line(RowContent::playlist(&entry), colors)
            .to_string()
            .contains(expected)
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
    assert!(terminal_text(&terminal).contains(expected));
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
