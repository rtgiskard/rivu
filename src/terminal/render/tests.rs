use super::*;
use crate::terminal::tests::*;
use ratatui::backend::TestBackend;
use rivu_protocol::response::QueryRevisions;

#[test]
fn details_loading_and_errors_preserve_scroll_until_fresh_track_is_rendered() {
    let i18n = Localizer::new("en-US");
    let colors = TerminalColors::new(crate::theme::Theme::default());
    let mut details = Details {
        target: Some(2),
        source_label: "terminal-queue",
        state: DetailState::Ready(Box::new(full_track(2))),
        scroll: 4,
        ..Details::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
    terminal
        .draw(|frame| draw_details(frame, frame.area(), &mut details, &i18n, colors))
        .unwrap();
    let position = (details.scroll, details.max_scroll);
    assert_eq!(position.0, 4);
    for state in [
        DetailState::Loading,
        DetailState::Error("retry failed".into()),
    ] {
        details.state = state;
        terminal
            .draw(|frame| draw_details(frame, frame.area(), &mut details, &i18n, colors))
            .unwrap();
        assert_eq!((details.scroll, details.max_scroll), position);
    }
    details.state = DetailState::Ready(Box::new(full_track(2)));
    terminal
        .draw(|frame| draw_details(frame, frame.area(), &mut details, &i18n, colors))
        .unwrap();
    assert_eq!((details.scroll, details.max_scroll), position);
}

#[test]
fn snapshot_theme_changes_recolor_main_help_and_details_without_changing_content() {
    for overlay in 0..3 {
        let mut tui = TuiState::new(TuiSnapshot::default(), Arc::new(Localizer::new("en-US")));
        tui.ui.show_help = overlay == 1;
        if overlay == 2 {
            tui.ui.details_active = true;
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| draw(frame, &tui.snapshot, &mut tui.ui))
            .unwrap();
        let content = terminal_text(&terminal);
        for theme in crate::theme::Theme::ALL {
            let mut next = tui.snapshot.clone();
            next.system.ui_theme = theme;
            next.system.revision += 1;
            tui.apply_snapshot(next);
            terminal
                .draw(|frame| draw(frame, &tui.snapshot, &mut tui.ui))
                .unwrap();
            assert_eq!(terminal_text(&terminal), content);
            let colors = TerminalColors::new(theme);
            let cell = &terminal.backend().buffer()[(79, 11)];
            assert_eq!(cell.bg, colors.bg);
            if overlay == 2 {
                assert_eq!(terminal.backend().buffer()[(0, 2)].fg, colors.secondary);
            }
        }
    }
}

#[test]
fn details_preserve_missing_paths_unknowns_zeroes_and_responsive_order() {
    let i18n = Localizer::new("en-US");
    let colors = TerminalColors::new(crate::theme::Theme::default());
    for width in [40, 73, 74, 96, 120] {
        let mut details = Details {
            target: Some(2),
            source_label: "terminal-queue",
            state: DetailState::Ready(Box::new(full_track(2))),
            ..Details::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(width, 100)).unwrap();
        terminal
            .draw(|frame| draw_details(frame, frame.area(), &mut details, &i18n, colors))
            .unwrap();
        let text = terminal_text(&terminal);
        assert!(text.contains("/music/夜航.flac"));
        assert!(text.contains("≈ 1200 kbps"));
        assert!(text.contains("96000 Hz"));
        assert!(text.contains("—"));
        let buffer = terminal.backend().buffer();
        let locate = |needle: &str| {
            (0..100)
                .find_map(|y| {
                    let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                    row.find(needle).map(|x| (x, y))
                })
                .unwrap()
        };
        let disc = locate("Disc");
        let track = locate("Track");
        assert_eq!(disc.0, 2);
        if width >= 74 {
            assert_eq!(disc.1, track.1);
            assert_eq!(track.0, 40);
        } else {
            assert!(disc.1 < track.1);
        }
        let missing = locate("Missing");
        assert_eq!(buffer[(missing.0 as u16, missing.1)].fg, colors.warning);
        let count = locate("Play count");
        assert_eq!(buffer[(count.0 as u16 + 2, count.1 + 1)].symbol(), "0");
    }
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
        let mut ui = UiState::new(Arc::new(Localizer::new(language)));
        ui.show_help = true;
        let state = TuiSnapshot::default();
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert!(ui.help_max_scroll > 0);
        assert!(help_bindings(KeyboardContext::TuiQueue).any(|row| row.action == Action::Details));
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
        let mut search = LibrarySearch {
            query: query.into(),
            ..LibrarySearch::default()
        };
        let i18n = Localizer::new(language);
        for width in [1, 8, 18, 24, 60] {
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
            terminal
                .draw(|frame| {
                    draw_search(
                        frame,
                        frame.area(),
                        &mut search,
                        &i18n,
                        TerminalColors::new(crate::theme::Theme::default()),
                    )
                })
                .unwrap();
            let prefix = format!(" 󰍉 {} › ", i18n.text("terminal-search"));
            if width as usize > Span::raw(prefix.as_str()).width() {
                assert!(terminal.backend().cursor_visible());
                let cursor = terminal.backend().cursor_position();
                assert!(cursor.x < width);
                assert_eq!(cursor.y, 0);
            }
            assert_eq!(search.query, query);
        }
        search.focus = SearchFocus::Results;
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| {
                draw_search(
                    frame,
                    frame.area(),
                    &mut search,
                    &i18n,
                    TerminalColors::new(crate::theme::Theme::default()),
                )
            })
            .unwrap();
        assert!(!terminal.backend().cursor_visible());
    }
    assert_eq!(query_tail(query, 5), ("e\u{301}夜航", 5));
    assert_eq!(query_tail(query, 1), ("", 0));
}

#[test]
fn metadata_columns_clip_whole_graphemes_without_shifting_duration() {
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
                TerminalColors::new(crate::theme::Theme::default()),
            );
        assert_eq!(line.width(), width as usize);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let (before_duration, _) = text.split_once("4:03").unwrap();
        assert_eq!(Span::raw(before_duration).width(), width as usize - 11);
        assert!(text.contains('…'));
        assert!(text.contains('󰓎') && text.contains('󰩋'));
        assert!(!text.contains('\u{fffd}'));
    }
    assert_eq!(cell_text("春风", 3, false), "春…");
    assert_eq!(cell_text("e\u{301}夜航", 4, false), "e\u{301}夜…");
}

#[test]
fn browser_modes_leave_all_rows_below_the_count_header_for_items() {
    let i18n = Localizer::new("en-US");
    for mode in [
        BrowserMode::Tracks,
        BrowserMode::PlaylistTracks,
        BrowserMode::UnfocusedTracks,
        BrowserMode::Labels,
    ] {
        let mut page = PagedList::default();
        page.insert(0, 2, 2, vec![track(0), track(1)]);
        for width in [1, 2, 3, 4, 12, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
            terminal
                .draw(|frame| {
                    draw_paged_list(
                        frame,
                        frame.area(),
                        "Items".into(),
                        &mut page,
                        |track| RowContent::track(track),
                        &i18n,
                        BrowserStyle {
                            mode,
                            colors: TerminalColors::new(crate::theme::Theme::default()),
                        },
                    );
                })
                .unwrap();
            let text = terminal_text(&terminal);
            let lines = text.lines().collect::<Vec<_>>();
            let buffer = terminal.backend().buffer();
            for y in 0..3 {
                assert_eq!(buffer[(0, y)].symbol(), " ");
                assert_eq!(buffer[(width - 1, y)].symbol(), " ");
            }
            if width >= 12 {
                let indent = if matches!(mode, BrowserMode::PlaylistTracks) {
                    2
                } else {
                    1
                };
                assert!(lines[0].starts_with(" Items"), "{text}");
                for (index, line) in lines[1..].iter().enumerate() {
                    assert!(
                        line.starts_with(&format!("{}row-{index}", " ".repeat(indent))),
                        "{text}"
                    );
                }
            }
        }
    }
}

#[test]
fn compact_attributes_align_loaded_rows_and_states_do_not_shift_titles() {
    let mut first = track(0);
    first.title = "first-title".into();
    first.codec = "FLAC".into();
    first.duration = Some(243.0);
    let mut second = track(1);
    second.artist = "longer artist".into();
    second.codec = "PCM_S16LE".into();
    second.duration = Some(6003.0);
    let columns = BrowserColumns::new(
        80,
        true,
        [&first, &second].into_iter().map(RowContent::track),
    );
    let text = |line: Line<'_>| {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    let colors = TerminalColors::new(crate::theme::Theme::default());
    let first_text = text(columns.line(RowContent::track(&first), colors));
    let second_text = text(columns.line(RowContent::track(&second), colors));
    assert!(first_text.contains("flac") && !first_text.contains("FLAC"));
    assert!(second_text.contains("pcm_s16le"));
    assert_eq!(first_text.find("flac"), second_text.find("pcm_s16le"));
    assert!(first_text.trim_end().ends_with("4:03"));
    assert!(second_text.trim_end().ends_with("100:03"));
    for marked in [false, true] {
        first.favorite = marked;
        first.missing = marked;
        let mut row = RowContent::track(&first);
        if marked {
            row.marker = "";
        }
        let line = columns.line(row, colors);
        assert_eq!(line.width(), 80);
        let rendered = text(line);
        assert!(rendered.starts_with("first-title"));
        if marked {
            assert!(rendered.trim_end().ends_with(" 󰓎 󰩋"));
        }
    }
    for marker in ["󰉋", "󰲋"] {
        for width in [3, 12, 80] {
            let columns = BrowserColumns::new(width, false, std::iter::empty());
            let line = columns.line(RowContent::label(Cow::Borrowed("name"), marker), colors);
            assert_eq!(line.width(), width as usize);
            let rendered = text(line);
            assert!(rendered.starts_with(&format!("{marker} ")));
            if width >= 12 {
                assert!(rendered.starts_with(&format!("{} name", cell_text(marker, 2, false))));
            }
        }
    }
    for width in 0..=100 {
        let columns = BrowserColumns::new(width, true, std::iter::once(RowContent::track(&first)));
        let mut row = RowContent::track(&first);
        row.file_icon = true;
        let line = columns.line(row, colors);
        assert_eq!(line.width(), width as usize);
        if columns.title >= 3 {
            assert!(text(line).starts_with("󰎇 "));
        }
    }
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
                TerminalColors::new(crate::theme::Theme::default()),
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
                    &Localizer::new("en-US"),
                    TerminalColors::new(state.system.ui_theme),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let marker_x = (0..buffer.area.width)
            .find(|&x| buffer[(x, 1)].symbol() == symbol)
            .expect("current queue occurrence has its playback marker");
        assert_eq!(buffer[(marker_x, 2)].symbol(), " ");
        let colors = TerminalColors::new(state.system.ui_theme);
        assert_eq!(buffer[(marker_x, 1)].fg, colors.accent);
        assert_ne!(buffer[(marker_x, 1)].bg, colors.selected);
        assert_eq!(buffer[(marker_x, 2)].bg, colors.selected);
        assert_eq!(page.selected().unwrap().id, 18);
    }
}

#[test]
fn localized_playlist_rendering_preserves_user_metadata() {
    let mut detail = PlaylistDetail {
        playlist_id: 1,
        name: "我的列表".into(),
        entries: PagedList::default(),
        entry_anchor: None,
        selected_entry_id: None,
    };
    detail.entries.set_viewport(4);
    detail.entries.insert(
        0,
        4,
        1,
        vec![PlaylistEntryRow {
            position: 0,
            offset: 0,
            id: 1,
            track_id: 1,
            artist: "原始艺人".into(),
            title: "Original title".into(),
            album: "原始专辑".into(),
            duration: Some(243.0),
            codec: "FLAC".into(),
            path: Default::default(),
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: None,
            bitrate_bps: None,
            bitrate_exact: false,
            cue_number: None,
            favorite: false,
            missing: true,
        }],
    );
    for language in ["en-US", "zh-CN"] {
        let i18n = Localizer::new(language);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| {
                draw_playlist_detail(
                    frame,
                    frame.area(),
                    &mut detail,
                    &i18n,
                    TerminalColors::new(crate::theme::Theme::default()),
                );
            })
            .unwrap();
        let text = terminal_text(&terminal);
        for metadata in ["我的列表", "原始艺人", "Original title"] {
            assert!(text.contains(metadata), "{text}");
        }
    }
}

#[test]
fn queue_mutations_relocate_selected_occurrence_and_clamp_deleted_selection() {
    let mut state = TuiSnapshot::default();
    state.queue.total = 12;
    state.system.page_size = 3;
    let mut tui = TuiState::new(state.clone(), Arc::new(Localizer::new("en-US")));
    tui.ui.queue.selection.select(Some(8));
    load_queue(&mut tui.ui, &state);
    state.queue.total = 11;
    state.queue.revision = 1;
    tui.apply_snapshot(state.clone());
    assert!(tui.ui.queue.pages.is_empty());
    let request = tui.ui.query_request(&state).unwrap();
    assert_eq!(request.kind, QueryKind::QueueSelection(1008));
    assert!(
        matches!(request.query(), Query::QueueEntries { queue_ids, query: None } if queue_ids == vec![1008])
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
    assert_eq!(request.offset, 0);
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
    assert_eq!(request.kind, QueryKind::QueueSelection(1008));
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
    let height = match ui.view {
        View::Search(_) => 6,
        _ => 5,
    };
    let mut terminal = Terminal::new(TestBackend::new(60, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            match &mut ui.view {
                View::Search(search) => draw_search(
                    frame,
                    area,
                    search,
                    &ui.i18n,
                    TerminalColors::new(crate::theme::Theme::default()),
                ),
                View::Tree(tree) => draw_tree(
                    frame,
                    area,
                    tree,
                    &ui.i18n,
                    TerminalColors::new(crate::theme::Theme::default()),
                ),
                View::Playlists(playlists) => draw_playlists(
                    frame,
                    area,
                    playlists,
                    &ui.i18n,
                    TerminalColors::new(crate::theme::Theme::default()),
                ),
                View::PlaylistDetail { detail, .. } => draw_playlist_detail(
                    frame,
                    area,
                    detail,
                    &ui.i18n,
                    TerminalColors::new(crate::theme::Theme::default()),
                ),
                View::Queue => panic!("expected a queried view"),
            }
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
