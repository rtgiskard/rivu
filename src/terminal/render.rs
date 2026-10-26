use super::*;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, LineGauge, List, ListItem, Paragraph},
};
use rivu_protocol::model::RepeatMode;

pub(super) fn track_label<'a>(title: &'a str, artist: &'a str) -> Cow<'a, str> {
    if title.is_empty() {
        Cow::Borrowed(artist)
    } else if artist.is_empty() {
        Cow::Borrowed(title)
    } else {
        Cow::Owned(format!("{title} — {artist}"))
    }
}

#[derive(Clone, Copy)]
struct TerminalColors {
    bg: ratatui::style::Color,
    text: ratatui::style::Color,
    secondary: ratatui::style::Color,
    border: ratatui::style::Color,
    structure_border: ratatui::style::Color,
    disabled: ratatui::style::Color,
    accent: ratatui::style::Color,
    selected: ratatui::style::Color,
    warning: ratatui::style::Color,
    error: ratatui::style::Color,
}

impl TerminalColors {
    fn new(theme: crate::theme::Theme) -> Self {
        let palette = crate::theme::palette(theme);
        let rgb = |color: u32| {
            ratatui::style::Color::Rgb((color >> 16) as u8, (color >> 8) as u8, color as u8)
        };
        Self {
            bg: rgb(palette.bg),
            text: rgb(palette.text),
            secondary: rgb(palette.secondary),
            border: rgb(palette.border),
            structure_border: rgb(palette.structure_border),
            disabled: rgb(palette.disabled),
            accent: rgb(palette.accent),
            selected: rgb(palette.selected),
            warning: rgb(palette.warning),
            error: rgb(palette.error),
        }
    }
}

fn playback_symbol(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "",
        PlaybackStatus::Paused => "󰏤",
        PlaybackStatus::Stopped => "",
    }
}

fn list_selection_style(colors: TerminalColors) -> Style {
    Style::default().bg(colors.selected)
}

pub(super) fn draw_loading(frame: &mut ratatui::Frame<'_>, i18n: &Localizer) {
    let colors = TerminalColors::new(crate::theme::Theme::default());
    frame.render_widget(
        Paragraph::new(format!(" RIVU  {}", i18n.text("terminal-connecting")))
            .style(Style::default().fg(colors.text).bg(colors.bg)),
        frame.area(),
    );
}

fn wrap_details(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for source in text.lines() {
        let mut line = String::with_capacity(width);
        let mut cells = 0;
        for grapheme in source.graphemes(true) {
            let count = Span::raw(grapheme).width();
            if cells + count > width && !line.is_empty() {
                lines.push(line);
                line = String::with_capacity(width);
                cells = 0;
            }
            line.push_str(grapheme);
            cells += count;
        }
        lines.push(line);
    }
    lines
}

fn detail_value(value: &str, colors: TerminalColors) -> Span<'static> {
    if value.is_empty() {
        Span::styled("—", Style::default().fg(colors.secondary))
    } else {
        Span::styled(value.to_owned(), Style::default().fg(colors.text))
    }
}

fn detail_field(
    label: &str,
    value: &str,
    width: usize,
    colors: TerminalColors,
) -> Vec<Line<'static>> {
    let label_indent = width.min(2);
    let value_indent = width.min(4);
    let mut lines = Vec::new();
    for line in wrap_details(label, width.saturating_sub(label_indent)) {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(label_indent)),
            Span::styled(line, Style::default().fg(colors.secondary)),
        ]));
    }
    let value = if value.is_empty() { "—" } else { value };
    for line in wrap_details(value, width.saturating_sub(value_indent)) {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(value_indent)),
            detail_value(&line, colors),
        ]));
    }
    lines
}

fn detail_fields(
    lines: &mut Vec<Line<'static>>,
    fields: &[(&str, String)],
    width: usize,
    i18n: &Localizer,
    colors: TerminalColors,
) {
    const COLUMN_WIDTH: usize = 36;
    const COLUMN_GAP: usize = 2;
    for pair in fields.chunks(if width >= COLUMN_WIDTH * 2 + COLUMN_GAP {
        2
    } else {
        1
    }) {
        if pair.len() == 2 {
            let left_width = COLUMN_WIDTH + COLUMN_GAP;
            let left = detail_field(i18n.text(pair[0].0), &pair[0].1, COLUMN_WIDTH, colors);
            let right = detail_field(i18n.text(pair[1].0), &pair[1].1, COLUMN_WIDTH, colors);
            for index in 0..left.len().max(right.len()) {
                let mut row = left.get(index).cloned().unwrap_or_default();
                let padding = left_width.saturating_sub(row.width());
                row.spans.push(Span::raw(" ".repeat(padding)));
                if let Some(other) = right.get(index) {
                    row.spans.extend(other.spans.iter().cloned());
                }
                lines.push(row);
            }
        } else {
            lines.extend(detail_field(
                i18n.text(pair[0].0),
                &pair[0].1,
                width.min(COLUMN_WIDTH),
                colors,
            ));
        }
        lines.push(Line::default());
    }
}

fn page_body(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    colors: TerminalColors,
) -> Rect {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(title.to_owned()).style(
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        ),
        rows[0],
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(colors.structure_border)),
        rows[1],
    );
    rows[2]
}

fn draw_details(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    details: &mut Details,
    i18n: &Localizer,
    colors: TerminalColors,
) {
    let area = page_body(frame, area, i18n.text("terminal-details"), colors);
    let width = area.width as usize;
    let mut lines = Vec::new();
    if let Some(id) = details.target {
        if let DetailState::Ready(track) = &details.state {
            let title = if track.title.is_empty() {
                "—"
            } else {
                &track.title
            };
            for line in wrap_details(title, width) {
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default()
                        .fg(if track.title.is_empty() {
                            colors.secondary
                        } else {
                            colors.text
                        })
                        .add_modifier(Modifier::BOLD),
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            format!("{} · ID {id}", i18n.text(details.source_label)),
            Style::default().fg(colors.secondary),
        )));
    }
    match &details.state {
        DetailState::Container(text) => {
            lines.extend(wrap_details(text, width).into_iter().map(|line| {
                Line::from(Span::styled(
                    line,
                    Style::default()
                        .fg(colors.text)
                        .add_modifier(Modifier::BOLD),
                ))
            }));
            lines.push(Line::from(Span::styled(
                i18n.text(details.source_label).to_owned(),
                Style::default().fg(colors.secondary),
            )));
        }
        _ if details.target.is_none() => {
            lines.extend(
                wrap_details(i18n.text("terminal-details-no-target"), width)
                    .into_iter()
                    .map(|line| {
                        Line::from(Span::styled(line, Style::default().fg(colors.secondary)))
                    }),
            );
        }
        DetailState::Loading => lines.push(Line::from(Span::styled(
            i18n.text("terminal-details-loading").to_owned(),
            Style::default().fg(colors.secondary),
        ))),
        DetailState::Removed => lines.push(Line::from(Span::styled(
            i18n.text("terminal-details-removed").to_owned(),
            Style::default().fg(colors.warning),
        ))),
        DetailState::Error(error) => {
            lines.extend(
                wrap_details(error, width)
                    .into_iter()
                    .map(|line| Line::from(Span::styled(line, Style::default().fg(colors.error)))),
            );
        }
        DetailState::Ready(track) => {
            let mut status = vec![Span::styled(
                i18n.text(if track.missing {
                    "gl-missing"
                } else {
                    "gl-ready"
                })
                .to_owned(),
                Style::default().fg(if track.missing {
                    colors.warning
                } else {
                    colors.secondary
                }),
            )];
            if track.favorite {
                status.push(Span::styled(
                    format!(" · 󰓎 {}", i18n.text("terminal-favorite")),
                    Style::default().fg(colors.accent),
                ));
            }
            lines.push(Line::from(status));
            let group = |lines: &mut Vec<Line<'static>>, label: &str| {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    i18n.text(label).to_owned(),
                    Style::default().fg(colors.accent),
                )));
            };
            group(&mut lines, "terminal-details-tags");
            for (label, value) in [("gl-artist", &track.artist), ("gl-album", &track.album)] {
                lines.extend(detail_field(i18n.text(label), value, width, colors));
                lines.push(Line::default());
            }
            detail_fields(
                &mut lines,
                &[
                    (
                        "gl-disc",
                        track.disc_number.map(|n| n.to_string()).unwrap_or_default(),
                    ),
                    (
                        "gl-track",
                        track
                            .track_number
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                    ),
                    ("gl-release", track.release_date.clone().unwrap_or_default()),
                ],
                width,
                i18n,
                colors,
            );
            group(&mut lines, "terminal-details-audio");
            detail_fields(
                &mut lines,
                &[
                    ("gl-codec", track.codec.to_ascii_lowercase()),
                    (
                        "gl-sample-rate",
                        if track.sample_rate == 0 {
                            String::new()
                        } else {
                            format!("{} Hz", track.sample_rate)
                        },
                    ),
                    (
                        "gl-channels",
                        if track.channels == 0 {
                            String::new()
                        } else {
                            track.channels.to_string()
                        },
                    ),
                    (
                        "gl-duration",
                        track.duration.map(fmt_time).unwrap_or_default(),
                    ),
                    (
                        "gl-bitrate",
                        track
                            .bitrate_bps
                            .map(|n| {
                                format!(
                                    "{}{:.0} kbps",
                                    if track.bitrate_exact { "" } else { "≈ " },
                                    n as f64 / 1000.0
                                )
                            })
                            .unwrap_or_default(),
                    ),
                    (
                        "gl-bits-sample",
                        track
                            .bits_per_sample
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                    ),
                ],
                width,
                i18n,
                colors,
            );
            group(&mut lines, "gl-file");
            lines.extend(detail_field(
                i18n.text("gl-track-kind"),
                i18n.text(if track.cue.is_some() {
                    "gl-cue-segment"
                } else {
                    "gl-whole-file"
                }),
                width,
                colors,
            ));
            lines.push(Line::default());
            lines.extend(detail_field(
                i18n.text("gl-file-path"),
                &track.path.to_string_lossy(),
                width,
                colors,
            ));
            lines.push(Line::default());
            if let Some(cue) = &track.cue {
                let position = |frame: u64| {
                    let seconds = frame / 75;
                    format!("{:02}:{:02}:{:02}", seconds / 60, seconds % 60, frame % 75)
                };
                detail_fields(
                    &mut lines,
                    &[
                        ("gl-cue-sheet", cue.sheet.to_string_lossy().into_owned()),
                        ("gl-cue-number", cue.number.to_string()),
                        ("gl-cue-start", position(cue.start_frame)),
                        (
                            "gl-cue-end",
                            cue.end_frame
                                .map(position)
                                .unwrap_or_else(|| i18n.text("gl-file-end").to_owned()),
                        ),
                    ],
                    width,
                    i18n,
                    colors,
                );
                lines.extend(
                    wrap_details(i18n.text("gl-cue-range-note"), width)
                        .into_iter()
                        .map(|line| {
                            Line::from(Span::styled(line, Style::default().fg(colors.secondary)))
                        }),
                );
                lines.push(Line::default());
            }
            let decoder = match track.decoder_hint {
                rivu_format::media::DecoderHint::Auto => i18n.text("gl-auto"),
                rivu_format::media::DecoderHint::Native => i18n.text("gl-native"),
                rivu_format::media::DecoderHint::Ffmpeg => "FFmpeg",
            };
            lines.extend(detail_field(
                i18n.text("gl-decoder-hint"),
                decoder,
                width,
                colors,
            ));
            group(&mut lines, "terminal-details-history");
            detail_fields(
                &mut lines,
                &[
                    ("gl-play-count", track.play_count.to_string()),
                    (
                        "gl-last-played",
                        track
                            .last_played
                            .and_then(|time| chrono::DateTime::from_timestamp(time, 0))
                            .map(|time| {
                                time.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M:%S")
                                    .to_string()
                            })
                            .unwrap_or_default(),
                    ),
                ],
                width,
                i18n,
                colors,
            );
        }
    }
    // Loading and transient failures must not erase the retained track position.
    let scroll = if matches!(details.state, DetailState::Loading | DetailState::Error(_)) {
        0
    } else {
        details.max_scroll = lines.len().saturating_sub(area.height as usize);
        details.scroll = details.scroll.min(details.max_scroll);
        details.scroll
    };
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(scroll)
                .take(area.height as usize)
                .collect::<Vec<_>>(),
        ),
        area,
    );
}

fn help_group(action: Action) -> usize {
    match action {
        Action::Help | Action::Close => 3,
        Action::Traverse
        | Action::Up
        | Action::Down
        | Action::First
        | Action::Last
        | Action::PageUp
        | Action::PageDown
        | Action::Activate
        | Action::Expand
        | Action::Parent
        | Action::Cancel => 0,
        Action::Toggle
        | Action::Next
        | Action::Previous
        | Action::VolumeDown
        | Action::VolumeUp
        | Action::Repeat
        | Action::Shuffle
        | Action::SeekBackward
        | Action::SeekForward
        | Action::SeekStart
        | Action::SeekEnd => 1,
        _ => 2,
    }
}

fn help_order(action: Action) -> u8 {
    match action {
        Action::Traverse | Action::Toggle | Action::Search | Action::Help => 0,
        Action::Up | Action::Down | Action::Next | Action::Previous | Action::Details => 1,
        Action::PageUp
        | Action::PageDown
        | Action::SeekBackward
        | Action::SeekForward
        | Action::Backspace => 2,
        Action::First | Action::Last | Action::SeekStart | Action::SeekEnd | Action::Enqueue => 3,
        Action::Expand | Action::VolumeDown | Action::VolumeUp | Action::Favorite => 4,
        Action::Activate | Action::Repeat | Action::Remove => 5,
        Action::Parent | Action::Shuffle => 6,
        _ => 7,
    }
}

fn draw_help(frame: &mut ratatui::Frame<'_>, area: Rect, ui: &mut UiState, colors: TerminalColors) {
    let context = ui.keyboard_context();
    let context_label = match context {
        KeyboardContext::TuiQueue => "terminal-queue",
        KeyboardContext::TuiPlaylists | KeyboardContext::TuiPlaylistTracks => "terminal-playlists",
        KeyboardContext::TuiSearchQuery | KeyboardContext::TuiSearchResults => "terminal-search",
        _ => "terminal-library",
    };
    let title = format!(
        "{} — {}",
        ui.i18n.text("terminal-keys"),
        ui.i18n.text(context_label)
    );
    let body = page_body(frame, area, &title, colors);
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(body);
    let inner = rows[0];
    frame.render_widget(
        Paragraph::new(ui.i18n.text("terminal-help-footer"))
            .style(Style::default().fg(colors.secondary)),
        rows[1],
    );
    ui.page_size = inner.height.max(1) as usize;
    let width = inner.width as usize;
    let mut bindings: Vec<_> = help_bindings(context).collect();
    bindings.sort_by_key(|binding| help_order(binding.action));
    let key_width = bindings
        .iter()
        .map(|binding| Span::raw(binding.label()).width())
        .max()
        .unwrap_or(0);
    let indent = 2.min(width.saturating_sub(1));
    let columns = width >= indent + key_width + 10;
    let description_indent = if columns {
        indent + key_width + 2
    } else {
        (indent + 2).min(width.saturating_sub(1))
    };
    let mut lines = Vec::new();
    for (group, label) in [
        "terminal-help-navigation",
        "terminal-help-playback",
        "terminal-help-page",
        "terminal-help-exit",
    ]
    .into_iter()
    .enumerate()
    {
        if !bindings
            .iter()
            .any(|binding| help_group(binding.action) == group)
        {
            continue;
        }
        lines.push(Line::from(Span::styled(
            ui.i18n.text(label).to_owned(),
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for binding in bindings
            .iter()
            .filter(|binding| help_group(binding.action) == group)
        {
            let key = binding.label();
            let descriptions = wrap_details(
                ui.i18n.text(binding.description),
                width.saturating_sub(description_indent).max(1),
            );
            if columns {
                for (index, description) in descriptions.into_iter().enumerate() {
                    let prefix = if index == 0 {
                        format!(
                            "{}{}{}",
                            " ".repeat(indent),
                            key,
                            " ".repeat(
                                key_width.saturating_sub(Span::raw(key.as_str()).width()) + 2
                            )
                        )
                    } else {
                        " ".repeat(description_indent)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(colors.text)),
                        Span::styled(description, Style::default().fg(colors.secondary)),
                    ]));
                }
            } else {
                for part in wrap_details(&key, width.saturating_sub(indent).max(1)) {
                    lines.push(Line::from(Span::styled(
                        format!("{}{part}", " ".repeat(indent)),
                        Style::default().fg(colors.text),
                    )));
                }
                for description in descriptions {
                    lines.push(Line::from(Span::styled(
                        format!("{}{description}", " ".repeat(description_indent)),
                        Style::default().fg(colors.secondary),
                    )));
                }
            }
        }
        lines.push(Line::default());
    }
    ui.help_max_scroll = lines.len().saturating_sub(inner.height as usize);
    ui.help_scroll = ui.help_scroll.min(ui.help_max_scroll);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(ui.help_scroll)
                .take(inner.height as usize)
                .collect::<Vec<_>>(),
        ),
        inner,
    );
}

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, state: &TuiSnapshot, ui: &mut UiState) {
    let colors = TerminalColors::new(state.system.ui_theme);
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(colors.text).bg(colors.bg)),
        area,
    );
    if ui.details_active {
        draw_details(frame, area, &mut ui.details, &ui.i18n, colors);
        ui.page_size = area.height.saturating_sub(2).max(1) as usize;
        return;
    }
    if ui.show_help {
        draw_help(frame, area, ui, colors);
        return;
    }
    let area = area.inner(ratatui::layout::Margin::new(1, 0));
    let spacing = u16::from(area.height >= 16);
    let outer = Layout::vertical([
        Constraint::Length(1 + spacing),
        Constraint::Min(1),
        Constraint::Length(2 + spacing),
        Constraint::Length(1 + spacing),
    ])
    .split(area);
    if spacing != 0 {
        frame.render_widget(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(colors.structure_border)),
            outer[0],
        );
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(colors.structure_border)),
            outer[2],
        );
    }
    let navigation = Rect {
        height: 1.min(outer[0].height),
        ..outer[0]
    };
    let playback = Rect {
        y: outer[2].y + spacing.min(outer[2].height),
        height: outer[2].height.saturating_sub(spacing),
        ..outer[2]
    };
    let status = Rect {
        height: 1.min(outer[3].height),
        ..outer[3]
    };
    let i18n = &ui.i18n;
    let active_tab = ui.view.tab();
    let mut tabs = Vec::new();
    for (tab, label) in [
        (TopTab::Queue, "terminal-queue"),
        (TopTab::Playlists, "terminal-playlists"),
        (TopTab::Library, "terminal-library"),
    ] {
        let style = if tab == active_tab {
            Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors.secondary)
        };
        tabs.push(Span::styled(i18n.text(label), style));
        tabs.push(Span::raw("  "));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), navigation);
    ui.page_size = outer[1]
        .height
        .saturating_sub(match ui.view {
            View::Search(_) => 2,
            _ => 1,
        })
        .max(1) as usize;
    match &mut ui.view {
        View::Search(search) => draw_search(frame, outer[1], search, i18n, colors),
        View::Tree(tree) => draw_tree(frame, outer[1], tree, i18n, colors),
        View::PlaylistDetail { detail, .. } => {
            draw_playlist_detail(frame, outer[1], detail, i18n, colors)
        }
        View::Playlists(playlists) => draw_playlists(frame, outer[1], playlists, i18n, colors),
        View::Queue => draw_queue(frame, outer[1], state, &mut ui.queue, i18n, colors),
    }
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(playback);
    let mut now = Vec::with_capacity(3);
    if let Some(track) = state.current_track() {
        now.push(Span::styled(
            format!(" {}  ", playback_symbol(state.playback.status)),
            Style::default().fg(colors.accent),
        ));
        now.push(Span::raw(track.title.as_str()));
        if !track.artist.is_empty() {
            now.push(Span::styled(
                format!(
                    "{}{}",
                    if track.title.is_empty() { "" } else { " — " },
                    track.artist
                ),
                Style::default().fg(colors.secondary),
            ));
        }
    } else {
        now.push(Span::raw(" "));
        now.push(Span::styled(
            i18n.text("terminal-nothing-playing"),
            Style::default().fg(colors.secondary),
        ));
    }
    let metadata = state
        .current_track()
        .filter(|_| area.width >= 90)
        .map(|track| {
            let mut metadata = track.codec.to_ascii_lowercase();
            if track.sample_rate != 0 {
                if !metadata.is_empty() {
                    metadata.push_str(" · ");
                }
                metadata.push_str(&format!("{:.1} kHz", track.sample_rate as f64 / 1000.0));
            }
            if let Some(bits) = track.bits_per_sample {
                if !metadata.is_empty() {
                    metadata.push_str(" · ");
                }
                metadata.push_str(&format!("{bits} bit"));
            }
            metadata
        })
        .unwrap_or_default();
    let now_columns = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(Span::raw(metadata.as_str()).width() as u16),
    ])
    .split(rows[0]);
    frame.render_widget(Paragraph::new(Line::from(now)), now_columns[0]);
    frame.render_widget(
        Paragraph::new(metadata).style(Style::default().fg(colors.secondary)),
        now_columns[1],
    );
    let position = state.playback.position.max(0.0);
    let duration = state
        .playback
        .duration
        .filter(|duration| duration.is_finite() && *duration > 0.0);
    let ratio = duration.map_or(0.0, |duration| (position / duration).clamp(0.0, 1.0));
    let time = format!(
        " {} / {} ",
        fmt_time(position),
        duration.map_or_else(|| "–:––".into(), fmt_time)
    );
    let progress = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(Line::from(time.as_str()).width() as u16),
    ])
    .split(rows[1]);
    frame.render_widget(
        LineGauge::default()
            .label("")
            .filled_style(Style::default().fg(colors.accent))
            .unfilled_style(Style::default().fg(colors.border))
            .ratio(ratio),
        progress[0],
    );
    frame.render_widget(
        Paragraph::new(time).style(Style::default().fg(colors.secondary)),
        progress[1],
    );
    draw_status(frame, status, ui, state, colors);
}

fn draw_status(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    ui: &UiState,
    state: &TuiSnapshot,
    colors: TerminalColors,
) {
    let volume = (state.playback.volume * 100.0).round() as u8;
    let volume_icon = if state.playback.volume == 0.0 {
        "󰖁"
    } else {
        "󰕾"
    };
    let (repeat_icon, repeat_color) = match state.playback.repeat {
        RepeatMode::Off => ("󰑖", colors.disabled),
        RepeatMode::All => ("󰑖", colors.accent),
        RepeatMode::One => ("󰑘", colors.accent),
    };
    let modes = Line::from(vec![
        Span::styled(
            format!(" {volume_icon}  {volume}%  "),
            Style::default().fg(if state.playback.volume == 0.0 {
                colors.disabled
            } else {
                colors.accent
            }),
        ),
        Span::styled(
            "󰒟",
            Style::default().fg(if state.playback.shuffle {
                colors.accent
            } else {
                colors.disabled
            }),
        ),
        Span::raw("  "),
        Span::styled(repeat_icon, Style::default().fg(repeat_color)),
        Span::raw("  "),
    ]);
    let columns = Layout::horizontal([
        Constraint::Length(modes.width().min(area.width.saturating_sub(8) as usize) as u16),
        Constraint::Min(0),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(modes), columns[0]);
    if let Some(notice) = &ui.message {
        let (symbol, color) = match notice.kind {
            NoticeKind::Information => ("", colors.secondary),
            NoticeKind::Warning => ("", colors.warning),
            NoticeKind::Error => ("", colors.error),
        };
        let text = format!("{symbol} {}", notice.text);
        frame.render_widget(
            Paragraph::new(cell_text(&text, columns[1].width as usize, true))
                .style(Style::default().fg(color)),
            columns[1],
        );
    } else if state.system.scanning || state.system.scan_progress.is_some() {
        let color = if state.system.scan_progress.as_ref().is_some_and(|progress| {
            progress.phase == rivu_protocol::model::ScanPhase::Failed || progress.errors != 0
        }) {
            colors.error
        } else {
            colors.secondary
        };
        let text = if let Some(progress) = &state.system.scan_progress {
            let phase = ui.i18n.text(match progress.phase {
                rivu_protocol::model::ScanPhase::Discovering => "gl-scan-discovering",
                rivu_protocol::model::ScanPhase::ReadingMetadata => "gl-scan-metadata",
                rivu_protocol::model::ScanPhase::Saving => "gl-scan-saving",
                rivu_protocol::model::ScanPhase::Completed => "gl-scan-completed",
                rivu_protocol::model::ScanPhase::Failed => "gl-scan-failed",
                rivu_protocol::model::ScanPhase::Cancelled => "gl-scan-cancelled",
            });
            format!(
                "{phase} · {}/{} · !{} · {}",
                progress.processed,
                progress
                    .total
                    .map(|total| total.to_string())
                    .unwrap_or_else(|| "?".into()),
                progress.errors,
                state.system.scan_message
            )
        } else {
            state.system.scan_message.clone()
        };
        frame.render_widget(
            Paragraph::new(cell_text(&text, columns[1].width as usize, true))
                .style(Style::default().fg(color)),
            columns[1],
        );
    } else if let Some(error) = &state.system.last_error {
        frame.render_widget(
            Paragraph::new(cell_text(error, columns[1].width as usize, true))
                .style(Style::default().fg(colors.error)),
            columns[1],
        );
    } else if matches!(ui.view, View::Queue) && ui.startup_hint_deadline.is_some() {
        frame.render_widget(
            Paragraph::new(cell_text(
                ui.i18n.text("terminal-startup-help"),
                columns[1].width as usize,
                true,
            ))
            .style(Style::default().fg(colors.secondary)),
            columns[1],
        );
    }
}

pub(super) struct RowContent<'a> {
    pub(super) title: Cow<'a, str>,
    pub(super) artist: &'a str,
    pub(super) album: &'a str,
    pub(super) codec: &'a str,
    pub(super) duration: Option<f64>,
    pub(super) favorite: bool,
    pub(super) missing: bool,
    marker: &'static str,
    file_icon: bool,
    is_track: bool,
}

impl<'a> RowContent<'a> {
    pub(super) fn track(track: &'a LibraryRow) -> Self {
        Self {
            title: Cow::Borrowed(&track.title),
            artist: &track.artist,
            album: &track.album,
            codec: &track.codec,
            duration: track.duration,
            favorite: track.favorite,
            missing: track.missing,
            marker: " ",
            file_icon: false,
            is_track: true,
        }
    }

    pub(super) fn playlist(entry: &'a PlaylistEntryRow) -> Self {
        Self {
            title: Cow::Borrowed(&entry.title),
            artist: &entry.artist,
            album: &entry.album,
            codec: &entry.codec,
            duration: entry.duration,
            favorite: entry.favorite,
            missing: entry.missing,
            marker: " ",
            file_icon: false,
            is_track: true,
        }
    }

    pub(super) fn label(title: Cow<'a, str>, marker: &'static str) -> Self {
        Self {
            title,
            artist: "",
            album: "",
            codec: "",
            duration: None,
            favorite: false,
            missing: false,
            marker,
            file_icon: false,
            is_track: false,
        }
    }
}

#[derive(Clone, Copy)]
struct BrowserColumns {
    title: usize,
    artist: usize,
    album: usize,
    codec: usize,
    duration: usize,
    markers: usize,
}

impl BrowserColumns {
    fn new<'a>(width: u16, tracks: bool, rows: impl Iterator<Item = RowContent<'a>>) -> Self {
        let width = width as usize;
        let markers = if tracks && width >= 14 { 7 } else { 0 };
        let mut artist = 0;
        let mut album = 0;
        let mut codec = 0;
        let mut duration = 0;
        if tracks {
            // The caller supplies only retained page items, never the full library.
            for row in rows.filter(|row| row.is_track) {
                artist = artist.max(Span::raw(row.artist).width().min(24));
                album = album.max(Span::raw(row.album).width().min(24));
                codec = codec.max(Span::raw(row.codec).width());
                let minutes = row.duration.map_or(0, |seconds| {
                    if seconds.is_finite() && seconds >= 0.0 {
                        seconds as u64 / 60
                    } else {
                        0
                    }
                });
                duration = duration.max(minutes.checked_ilog10().unwrap_or(0) as usize + 4);
            }
        }
        // Widths include a two-cell gap. Reserve readable title space before
        // admitting intrinsic attributes, then shrink or hide secondary text.
        for column in [&mut artist, &mut album, &mut codec, &mut duration] {
            if *column != 0 {
                *column += 2;
            }
        }
        let title_min = (width / 2).clamp(12, 24);
        let mut budget = width.saturating_sub(markers + title_min);
        for column in [&mut duration, &mut codec] {
            if *column > budget {
                *column = 0;
            }
            budget -= *column;
        }
        while artist + album > budget {
            if album >= artist && album > 6 {
                album -= 1;
            } else if artist > 6 {
                artist -= 1;
            } else if album != 0 {
                album = 0;
            } else {
                artist = 0;
            }
        }
        Self {
            title: width - markers - artist - album - codec - duration,
            artist,
            album,
            codec,
            duration,
            markers,
        }
    }

    fn line(self, row: RowContent<'_>, colors: TerminalColors) -> Line<'static> {
        let mut spans = Vec::with_capacity(13);
        let title = if self.artist == 0 {
            track_label(&row.title, row.artist)
        } else {
            Cow::Borrowed(row.title.as_ref())
        };
        let prefix = if row.file_icon {
            "󰎇"
        } else if !row.is_track {
            row.marker
        } else {
            " "
        };
        if !prefix.trim().is_empty() && self.title >= 3 {
            let prefix_width = 3;
            let padding = prefix_width - Span::raw(prefix).width();
            spans.push(Span::styled(
                format!("{prefix}{:padding$}", ""),
                Style::default().fg(colors.accent),
            ));
            spans.push(Span::raw(cell_text(
                &title,
                self.title.saturating_sub(prefix_width),
                false,
            )));
        } else {
            spans.push(Span::raw(cell_text(&title, self.title, false)));
        }
        for (text, width, codec) in [
            (row.artist, self.artist, false),
            (row.album, self.album, false),
            (row.codec, self.codec, true),
        ] {
            if width != 0 {
                let mut text = cell_text(text, width - 2, false);
                if codec {
                    text.make_ascii_lowercase();
                }
                spans.push(Span::raw("  "));
                spans.push(Span::styled(text, Style::default().fg(colors.secondary)));
            }
        }
        if self.duration != 0 {
            let duration = if row.is_track {
                row.duration.map_or_else(|| "–:––".into(), fmt_time)
            } else {
                String::new()
            };
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                cell_text(&duration, self.duration - 2, true),
                Style::default().fg(colors.secondary),
            ));
        }
        if self.markers != 0 {
            spans.push(Span::raw(" "));
            for (symbol, color) in [
                (if row.is_track { row.marker } else { " " }, colors.accent),
                (if row.favorite { "󰓎" } else { " " }, colors.secondary),
                (if row.missing { "󰩋" } else { " " }, colors.warning),
            ] {
                spans.push(Span::styled(
                    cell_text(symbol, 2, false),
                    Style::default().fg(color),
                ));
            }
        }
        Line::from(spans)
    }
}

// Clip whole graphemes by displayed cells. Embedded line breaks and Fluent's
// bidi isolation controls must not move table columns or hide the right edge.
fn cell_text(text: &str, width: usize, right: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let mut output = String::with_capacity(width);
    let mut used = 0;
    let mut clipped = false;
    for grapheme in text.graphemes(true) {
        if grapheme
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2068}' | '\u{2069}'))
        {
            continue;
        }
        let cells = Span::raw(grapheme).width();
        if used + cells > width {
            clipped = true;
            break;
        }
        output.push_str(grapheme);
        used += cells;
    }
    if clipped {
        while used >= width {
            let (index, last) = output
                .grapheme_indices(true)
                .next_back()
                .expect("occupied cell");
            used -= Span::raw(last).width();
            output.truncate(index);
        }
        output.push('…');
        used += 1;
    }
    if right {
        output.insert_str(0, &" ".repeat(width - used));
    } else {
        output.extend(std::iter::repeat_n(' ', width - used));
    }
    output
}

fn draw_queue(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &TuiSnapshot,
    queue: &mut PagedList<QueueRow>,
    i18n: &Localizer,
    colors: TerminalColors,
) {
    draw_paged_list(
        frame,
        area,
        i18n.text("terminal-queue").into(),
        queue,
        |row| {
            let mut content = RowContent::track(&row.track);
            if state.queue.current_id == Some(row.id) {
                content.marker = playback_symbol(state.playback.status);
            }
            content
        },
        i18n,
        BrowserStyle {
            mode: BrowserMode::Tracks,
            colors,
        },
    );
}

#[derive(Clone, Copy)]
enum BrowserMode {
    Tracks,
    PlaylistTracks,
    UnfocusedTracks,
    Labels,
}

#[derive(Clone, Copy)]
struct BrowserStyle {
    mode: BrowserMode,
    colors: TerminalColors,
}

fn draw_paged_list<T>(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: String,
    page: &mut PagedList<T>,
    row: impl for<'a> Fn(&'a T) -> RowContent<'a>,
    i18n: &Localizer,
    style: BrowserStyle,
) {
    let BrowserStyle { mode, colors } = style;
    let area = Rect::new(
        area.x + area.width.min(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    let tracks = !matches!(mode, BrowserMode::Labels);
    let focused = !matches!(mode, BrowserMode::UnfocusedTracks);
    page.set_viewport(area.height.saturating_sub(1) as usize);
    let loading = !page.loaded
        || (page.selection.offset()..page.visible_end()).any(|index| page.get(index).is_none());
    let title = if loading {
        i18n.format("terminal-page-loading", &[("title", title.as_str().into())])
    } else if page.total == 0 {
        i18n.format("terminal-page-empty", &[("title", title.as_str().into())])
    } else {
        title
    };
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    let mut items_area = rows[1];
    if matches!(mode, BrowserMode::PlaylistTracks) {
        items_area.x += items_area.width.min(1);
        items_area.width = items_area.width.saturating_sub(1);
    }
    let columns = BrowserColumns::new(
        items_area.width,
        tracks,
        page.pages.values().flatten().map(&row),
    );
    let count = if page.loaded {
        format!(
            " {}/{} ",
            page.selection.selected().map_or(0, |index| index + 1),
            page.total
        )
    } else {
        " … ".into()
    };
    let count_width = Span::raw(count.as_str()).width().min(area.width as usize);
    let title_line = Line::from(vec![
        Span::styled(
            cell_text(&title, area.width as usize - count_width, false),
            Style::default().fg(if focused {
                colors.accent
            } else {
                colors.secondary
            }),
        ),
        Span::styled(
            cell_text(&count, count_width, true),
            Style::default().fg(colors.secondary),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), rows[0]);
    let items = (page.selection.offset()..page.visible_end()).map(|index| {
        ListItem::new(page.get(index).map_or_else(
            || Line::from(cell_text("…", items_area.width as usize, false)),
            |item| columns.line(row(item), colors),
        ))
    });
    let mut selection = ListState::default().with_selected(
        page.selection
            .selected()
            .map(|index| index - page.selection.offset()),
    );
    frame.render_stateful_widget(
        List::new(items).highlight_style(if focused {
            list_selection_style(colors)
        } else {
            Style::default().bg(colors.selected)
        }),
        items_area,
        &mut selection,
    );
}

fn draw_playlist_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    detail: &mut PlaylistDetail,
    i18n: &Localizer,
    colors: TerminalColors,
) {
    draw_paged_list(
        frame,
        area,
        detail.name.clone(),
        &mut detail.entries,
        |entry| RowContent::playlist(entry),
        i18n,
        BrowserStyle {
            mode: BrowserMode::PlaylistTracks,
            colors,
        },
    );
}

fn draw_playlists(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    playlists: &mut PagedList<PlaylistSummary>,
    i18n: &Localizer,
    colors: TerminalColors,
) {
    draw_paged_list(
        frame,
        area,
        i18n.text("terminal-playlists").into(),
        playlists,
        |playlist| {
            RowContent::label(
                Cow::Owned(i18n.format(
                    "terminal-playlist-count",
                    &[
                        ("name", playlist.name.as_str().into()),
                        ("count", playlist.entry_count.into()),
                    ],
                )),
                "󰲸",
            )
        },
        i18n,
        BrowserStyle {
            mode: BrowserMode::Labels,
            colors,
        },
    );
}

fn query_tail(query: &str, width: usize) -> (&str, usize) {
    let mut cells = 0;
    let mut start = query.len();
    for (index, grapheme) in query.grapheme_indices(true).rev() {
        let count = Span::raw(grapheme).width();
        if cells + count > width {
            break;
        }
        cells += count;
        start = index;
    }
    (&query[start..], cells)
}

fn draw_search(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    search: &mut LibrarySearch,
    i18n: &Localizer,
    colors: TerminalColors,
) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    let focused = search.focus == SearchFocus::Query;
    let prefix = format!(" 󰍉 {} › ", i18n.text("terminal-search"));
    let prefix_width = Line::from(prefix.as_str()).width().min(area.width as usize);
    let input_width = area.width as usize - prefix_width;
    let (tail, cursor) = query_tail(
        &search.query,
        input_width.saturating_sub(usize::from(focused)),
    );
    let line = Line::from(vec![
        Span::styled(
            prefix,
            Style::default().fg(if focused {
                colors.accent
            } else {
                colors.secondary
            }),
        ),
        Span::raw(tail),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(if focused {
            Style::default().bg(colors.selected)
        } else {
            Style::default()
        }),
        rows[0],
    );
    if focused && input_width > 0 && rows[0].height > 0 {
        frame.set_cursor_position((area.x + (prefix_width + cursor) as u16, rows[0].y));
    }
    draw_paged_list(
        frame,
        rows[1],
        i18n.text("terminal-library").into(),
        &mut search.results,
        |track| RowContent::track(track),
        i18n,
        BrowserStyle {
            mode: if focused {
                BrowserMode::UnfocusedTracks
            } else {
                BrowserMode::Tracks
            },
            colors,
        },
    );
}

fn draw_tree(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    tree: &mut LibraryTree,
    i18n: &Localizer,
    colors: TerminalColors,
) {
    let title = if tree.other_tracks {
        format!(
            "{} · {}{}{} · /{}{} · {}",
            i18n.text("library-other"),
            i18n.text(match tree.sort {
                rivu_protocol::model::LibrarySort::Id => "terminal-sort-id",
                rivu_protocol::model::LibrarySort::Album => "terminal-sort-album",
                rivu_protocol::model::LibrarySort::MostPlayed => "terminal-sort-most-played",
            }),
            if tree.favorite.is_some() { " ♥" } else { "" },
            if tree.missing.is_some() { " !" } else { "" },
            tree.query,
            if tree.editing_query { "▏" } else { "" },
            i18n.text("terminal-other-tracks-help")
        )
    } else if tree.directory.as_os_str().is_empty() {
        i18n.text("terminal-library-tree").into()
    } else {
        tree.directory.to_string_lossy().into_owned()
    };
    let other_tracks = tree.other_tracks;
    let other_count = tree.other_count;
    draw_paged_list(
        frame,
        area,
        title,
        &mut tree.entries,
        |entry| match entry {
            DirectoryRow::OtherTracks => RowContent::label(
                match other_count {
                    Some(count) => format!("{} ({count})", i18n.text("library-other")).into(),
                    None => i18n.text("library-other").to_owned().into(),
                },
                "󰒟",
            ),
            DirectoryRow::Directory { path } => RowContent::label(
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy(),
                "󰉋",
            ),
            DirectoryRow::Track(track) => {
                let mut content = RowContent::track(track);
                if other_tracks {
                    content.title = format!(
                        "{} · {}",
                        track.title,
                        track.path.parent().unwrap_or(&track.path).display()
                    )
                    .into();
                }
                content.file_icon = true;
                content
            }
        },
        i18n,
        BrowserStyle {
            mode: BrowserMode::Tracks,
            colors,
        },
    );
    if other_tracks
        && tree.entries.loaded
        && tree.entries.total == 0
        && tree.query.is_empty()
        && tree.favorite.is_none()
        && tree.missing.is_none()
    {
        let body = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(1),
        );
        frame.render_widget(
            Paragraph::new(i18n.text("library-other-empty"))
                .style(Style::default().fg(colors.secondary))
                .wrap(ratatui::widgets::Wrap { trim: true }),
            body,
        );
    }
}

pub(super) fn fmt_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".into();
    }
    format!("{}:{:02}", (seconds as u64) / 60, (seconds as u64) % 60)
}

#[cfg(test)]
pub(super) mod tests;
