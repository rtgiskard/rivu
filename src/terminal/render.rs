use super::*;
use crate::model::RepeatMode;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, LineGauge, List, ListItem, Paragraph},
};

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
        let palette = theme.palette();
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
        PlaybackStatus::Playing => "󰐊",
        PlaybackStatus::Paused => "󰏤",
        PlaybackStatus::Stopped => "󰓛",
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

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, state: &TuiSnapshot, ui: &mut UiState) {
    let colors = TerminalColors::new(state.system.ui_theme);
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(colors.text).bg(colors.bg)),
        area,
    );
    if let Some(details) = &mut ui.details {
        let block = Block::default()
            .title(ui.i18n.text("terminal-details"))
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(colors.border))
            .title_style(Style::default().fg(colors.accent));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if details.width != inner.width {
            details.wrapped = wrap_details(&details.text, inner.width as usize);
            details.width = inner.width;
        }
        details.max_scroll = details.wrapped.len().saturating_sub(inner.height as usize);
        details.scroll = details.scroll.min(details.max_scroll);
        ui.page_size = inner.height.max(1) as usize;
        let lines: Vec<_> = details
            .wrapped
            .iter()
            .skip(details.scroll)
            .take(inner.height as usize)
            .map(|line| Line::from(line.as_str()))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if ui.show_help {
        let context = ui.keyboard_context();
        let items = help_bindings(context).map(|binding| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<22}", binding.label()),
                    Style::default().fg(colors.accent),
                ),
                Span::raw(ui.i18n.text(binding.description)),
            ]))
        });
        ui.page_size = area.height.saturating_sub(2).max(1) as usize;
        let mut block = Block::default()
            .title(ui.i18n.text("terminal-keys"))
            .title_style(Style::default().fg(colors.accent))
            .border_style(Style::default().fg(colors.border))
            .borders(Borders::TOP | Borders::BOTTOM);
        if context == KeyboardContext::TuiSearchQuery {
            block = block.title_bottom(ui.i18n.text("keyboard-query-input"));
        }
        let list = List::new(items)
            .block(block)
            .highlight_style(list_selection_style(colors));
        frame.render_stateful_widget(list, area, &mut ui.help_selection);
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
        let markers = if width >= 14 { 9 } else { 0 };
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
        if self.markers != 0 {
            for (symbol, color, width) in [
                (row.marker, colors.accent, 3),
                (if row.favorite { "" } else { " " }, colors.secondary, 3),
                (if row.missing { "" } else { " " }, colors.warning, 2),
            ] {
                spans.push(Span::styled(
                    cell_text(symbol, width, false),
                    Style::default().fg(color),
                ));
            }
            // Always separate the fixed marker slots from the title.
            spans.push(Span::raw(" "));
        }
        let title = if self.artist == 0 {
            track_label(&row.title, row.artist)
        } else {
            Cow::Borrowed(row.title.as_ref())
        };
        spans.push(Span::raw(cell_text(&title, self.title, false)));
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
    let columns = BrowserColumns::new(area.width, tracks, page.pages.values().flatten().map(&row));
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
            || Line::from(cell_text("…", area.width as usize, false)),
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
        rows[1],
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
            mode: BrowserMode::Tracks,
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
                "󰲋",
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
    let prefix = format!("  {} › ", i18n.text("terminal-search"));
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
    let title = if tree.directory.as_os_str().is_empty() {
        i18n.text("terminal-library-tree").into()
    } else {
        tree.directory.to_string_lossy().into_owned()
    };
    draw_paged_list(
        frame,
        area,
        title,
        &mut tree.entries,
        |entry| match entry {
            DirectoryRow::Directory { path } => RowContent::label(
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy(),
                "",
            ),
            DirectoryRow::Track(track) => RowContent::track(track),
        },
        i18n,
        BrowserStyle {
            mode: BrowserMode::Tracks,
            colors,
        },
    );
}

pub(super) fn fmt_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".into();
    }
    format!("{}:{:02}", (seconds as u64) / 60, (seconds as u64) % 60)
}

#[cfg(test)]
pub(super) mod tests;
