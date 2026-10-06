use super::{
    ACCENT, BORDER, Dragging, Field, GuiApp, HIGHLIGHT, HistorySort, ListFocus, MUTED, Measured, TEXT, UI_INSET,
    button, column, empty_state, format_time, icon_button, library::LibraryDrag, list_viewport,
    panel_header, panel_surface, panel_toolbar, row, row_text, track_row,
};
pub(super) use super::{TRACK_HEIGHT, caption, list_row};
use crate::{
    gui::QueueDrag,
    model::{Command, PlaybackStatus, RepeatMode},
};
use chrono::{DateTime, Local};
use gpui::{AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb, uniform_list};

fn metadata_pair(label: &'static str, value: impl Into<SharedString>) -> Div {
    row()
        .items_start()
        .gap_2()
        .flex_1()
        .min_w_0()
        .child(caption(label).w(px(92.)).flex_shrink_0())
        .child(div().flex_1().min_w_0().text_sm().truncate().child(value.into()))
}

fn metadata_row(first: Div, second: Div) -> Div {
    row()
        .items_start()
        .gap_4()
        .child(first.flex_1().min_w_0())
        .child(second.flex_1().min_w_0())
}

fn metadata_group(title: &'static str, content: impl IntoElement) -> Div {
    column()
        .gap_2()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .child(caption(title))
        .child(content)
}

impl GuiApp {
    fn last_played_text(&self, track_id: i64, stored_at: Option<i64>) -> String {
        let current = self
            .state
            .current_track()
            .is_some_and(|track| track.id == track_id);
        if current
            && self.state.playback.status == PlaybackStatus::Playing
            && self.state.playback.last_heard_at.is_some()
        {
            return "Last played · 0s ago (playing)".into();
        }

        let played_at = if current && self.state.playback.status == PlaybackStatus::Paused {
            self.state.playback.last_heard_at
        } else {
            stored_at
        };
        match played_at.and_then(|time| DateTime::from_timestamp(time, 0)) {
            Some(time) => format!(
                "Last played · {}{}",
                time.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"),
                if current && self.state.playback.status == PlaybackStatus::Paused {
                    " (paused)"
                } else {
                    ""
                }
            ),
            None => "Last played —".into(),
        }
    }
    pub(super) fn transport_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let track = self.state.current_track();
        let title = track
            .map(|track| track.title.clone())
            .unwrap_or_else(|| "Choose something to listen to".into());
        let artist = track.map(|track| track.artist.clone()).unwrap_or_default();
        let status = match self.state.playback.status {
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
            PlaybackStatus::Stopped => "Stopped",
        };
        let duration = self
            .state
            .playback
            .duration
            .or_else(|| track.and_then(|track| track.duration));
        let preview_position = self.seek_preview.unwrap_or(self.state.playback.position);
        let progress = duration
            .filter(|duration| *duration > 0.)
            .map_or(0., |duration| (preview_position / duration) as f32);
        let seek = self.slider(
            ("seek", panel_id),
            progress,
            Dragging::Seek(panel_id),
            Measured::Seek(panel_id),
            cx,
        );
        let volume = self.slider(
            ("volume", panel_id),
            self.state.playback.volume,
            Dragging::Volume(panel_id),
            Measured::Volume(panel_id),
            cx,
        );
        let controls = row()
            .gap_1()
            .flex_wrap()
            .justify_center()
            .child(
                icon_button(
                    ("shuffle", panel_id),
                    "󰒟",
                    if self.state.playback.shuffle {
                        "Shuffle: on (s)"
                    } else {
                        "Shuffle: off (s)"
                    },
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::Shuffle {
                                enabled: !this.state.playback.shuffle,
                            },
                            cx,
                        )
                    },
                )
                .size(gpui::rems(3.))
                .text_2xl()
                .when(self.state.playback.shuffle, |view| {
                    view.text_color(rgb(ACCENT)).border_color(rgb(ACCENT))
                }),
            )
            .child(
                icon_button(
                    ("previous", panel_id),
                    "󰒮",
                    "Previous track (p)",
                    cx,
                    |this, _, cx| this.send(Command::Previous, cx),
                )
                .size(gpui::rems(3.))
                .text_2xl(),
            )
            .child(
                icon_button(
                    ("toggle", panel_id),
                    if self.state.playback.status == PlaybackStatus::Playing {
                        "󰏤"
                    } else {
                        "󰐊"
                    },
                    if self.state.playback.status == PlaybackStatus::Playing {
                        "Pause (space)"
                    } else {
                        "Play (space)"
                    },
                    cx,
                    |this, _, cx| this.send(Command::Toggle, cx),
                )
                .size(gpui::rems(3.))
                .text_2xl()
                .bg(rgb(HIGHLIGHT))
                .text_color(rgb(ACCENT)),
            )
            .child(
                icon_button(
                    ("next", panel_id),
                    "󰒭",
                    "Next track (n)",
                    cx,
                    |this, _, cx| this.send(Command::Next, cx),
                )
                .size(gpui::rems(3.))
                .text_2xl(),
            )
            .child(
                icon_button(
                    ("repeat", panel_id),
                    if self.state.playback.repeat == RepeatMode::One {
                        "󰑘"
                    } else {
                        "󰑖"
                    },
                    match self.state.playback.repeat {
                        RepeatMode::Off => "Repeat: off (r)",
                        RepeatMode::All => "Repeat: all (r)",
                        RepeatMode::One => "Repeat: one (r)",
                    },
                    cx,
                    |this, _, cx| {
                        let mode = match this.state.playback.repeat {
                            RepeatMode::Off => RepeatMode::All,
                            RepeatMode::All => RepeatMode::One,
                            RepeatMode::One => RepeatMode::Off,
                        };
                        this.send(Command::Repeat { mode }, cx);
                    },
                )
                .size(gpui::rems(3.))
                .text_2xl()
                .when(self.state.playback.repeat != RepeatMode::Off, |view| {
                    view.text_color(rgb(ACCENT)).border_color(rgb(ACCENT))
                }),
            );
        let volume_control = row()
            .w(px(172.))
            .max_w(gpui::relative(1.))
            .flex_shrink_0()
            .child(
                div()
                    .id(("volume-label", panel_id))
                    .flex_shrink_0()
                    .text_lg()
                    .text_color(rgb(MUTED))
                    .child(if self.state.system.config.nerd_symbols {
                        "󰕾"
                    } else {
                        "♪"
                    })
                    .tooltip(|_, cx| {
                        cx.new(|_| super::ButtonTooltip {
                            text: "Volume ([ / ])".into(),
                        })
                        .into()
                    }),
            )
            .child(div().flex_1().min_w_0().child(volume))
            .child(
                caption(format!("{:.0}%", self.state.playback.volume * 100.))
                    .w(px(34.))
                    .flex_shrink_0(),
            );
        div()
            .id(("transport-panel", panel_id))
            .size_full()
            .overflow_y_scroll()
            .child(
                row()
                    .items_center()
                    .child(
                        div().size(px(128.)).flex_shrink_0().child(
                            self.default_album
                                .clone()
                                .cached(gpui::StyleRefinement::default().size_full()),
                        ),
                    )
                    .child(
                        column()
                            .flex_1()
                            .child(
                                row()
                                    .flex_shrink_0()
                                    .flex_wrap()
                                    .child(
                                        div().flex_1().min_w_0().text_xl().truncate().child(title),
                                    )
                                    .child(caption(artist).truncate())
                                    .child(caption(status)),
                            )
                            .child(
                                row()
                                    .flex_shrink_0()
                                    .child(
                                        div()
                                            .id(("progress-label", panel_id))
                                            .flex_shrink_0()
                                            .text_lg()
                                            .text_color(rgb(MUTED))
                                            .child(if self.state.system.config.nerd_symbols {
                                                "󰅐"
                                            } else {
                                                "◷"
                                            })
                                            .tooltip(|_, cx| {
                                                cx.new(|_| super::ButtonTooltip {
                                                    text: "Playback progress".into(),
                                                })
                                                .into()
                                            }),
                                    )
                                    .child(div().flex_1().min_w_0().child(seek))
                                    .child(
                                        caption(format!(
                                            "{} / {}",
                                            format_time(preview_position),
                                            duration.map_or("—".into(), format_time)
                                        ))
                                        .flex_shrink_0(),
                                    ),
                            )
                            .child(
                                row()
                                    .flex_shrink_0()
                                    .flex_wrap()
                                    .justify_between()
                                    .child(controls)
                                    .child(volume_control),
                            ),
                    ),
            )
            .into_any_element()
    }
    fn panel_field(&self, field: Field, label: &'static str) -> Div {
        column()
            .w_full()
            .flex_shrink_0()
            .gap_1()
            .child(caption(label))
            .child(self.input(field))
    }

    fn panel_selected_tracks(&self) -> Vec<i64> {
        self.selected.iter().copied().collect()
    }
    pub(super) fn panel_track_text(&self, track_id: i64) -> (String, String) {
        if let Some(track) = self
            .state
            .queue
            .tracks
            .iter()
            .find(|track| track.id == track_id)
        {
            let detail = format!(
                "{}{}{} · {}{}",
                track.artist,
                if track.artist.is_empty() || track.album.is_empty() {
                    ""
                } else {
                    " / "
                },
                track.album,
                format_time(track.duration.unwrap_or(0.)),
                if track.missing {
                    " · File missing"
                } else {
                    ""
                }
            );
            return (track.title.clone(), detail);
        }
        if let Some(row) = self
            .library_buffer
            .rows
            .iter()
            .find(|row| row.id == track_id)
            .or_else(|| self.ranking_rows.iter().find(|row| row.id == track_id))
        {
            return (
                row.title.clone(),
                format!(
                    "{}{}{} · {}",
                    row.artist,
                    if row.artist.is_empty() || row.album.is_empty() {
                        ""
                    } else {
                        " / "
                    },
                    row.album,
                    format_time(row.duration.unwrap_or(0.))
                ),
            );
        }
        if let Some(row) = self
            .playlist_entries
            .rows
            .iter()
            .find(|row| row.track_id == track_id)
        {
            return (
                row.title.clone(),
                format!(
                    "{}{}{}",
                    row.artist,
                    if row.artist.is_empty() || row.album.is_empty() {
                        ""
                    } else {
                        " / "
                    },
                    row.album
                ),
            );
        }
        ("Track".into(), format!("Track #{track_id}"))
    }

    pub(super) fn panel_error(&mut self, message: &'static str, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    fn library_scan_status(&self) -> Div {
        use crate::model::ScanPhase;
        let Some(progress) = self.state.system.scan_progress.as_ref() else {
            return column().child(caption(self.state.system.scan_message.clone()));
        };
        let phase = match progress.phase {
            ScanPhase::Discovering => "Discovering files",
            ScanPhase::ReadingMetadata => "Reading metadata",
            ScanPhase::Saving => "Saving library",
            ScanPhase::Completed => "Scan completed",
            ScanPhase::Failed => "Scan failed",
        };
        let count = match progress.total {
            Some(total) => format!("{} / {total} files", progress.processed),
            None => format!("{} files found", progress.processed),
        };
        let mut status = column().gap_1().flex_shrink_0().child(caption(format!(
            "{phase} · {count} · {} errors",
            progress.errors
        )));
        if let Some(total) = progress.total.filter(|total| *total > 0) {
            let fraction = (progress.processed as f32 / total as f32).clamp(0., 1.);
            status = status.child(div().w_full().h(px(3.)).bg(rgb(BORDER)).child(
                div().h_full().w(gpui::relative(fraction)).bg(rgb(
                    if progress.phase == ScanPhase::Failed {
                        super::ERROR
                    } else {
                        ACCENT
                    },
                )),
            ));
        }
        if let Some(path) = progress.path.as_ref() {
            status = status.child(caption(path.to_string_lossy().into_owned()).truncate());
        }
        if progress.phase == ScanPhase::Completed {
            status = status.child(caption(self.state.system.scan_message.clone()).truncate());
        }
        status
    }

    pub(super) fn library_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page_size = self.view_page_size();
        let (total, page) = if self.library_tree_active {
            let offset = self.directory_tree.root_offset();
            (self.directory_tree.root_total(), offset / page_size)
        } else {
            (self.library_buffer.total, self.library_page)
        };
        let page_count = total.div_ceil(page_size).max(1);
        let can_prev = page > 0;
        let can_next = page + 1 < page_count;
        let mut tools = panel_toolbar()
            .child(icon_button(
                ("library-view", panel_id),
                if self.library_tree_active {
                    "󰉋"
                } else {
                    "󰉢"
                },
                if self.library_tree_active {
                    "Switch to flat list view"
                } else {
                    "Switch to directory tree view"
                },
                cx,
                |this, _, cx| this.toggle_library_view(cx),
            ))
            .when(can_prev || can_next, |tools| {
                tools
                    .child(
                        icon_button(
                            ("library-page-prev", panel_id),
                            "󰒮",
                            "Browse earlier tracks",
                            cx,
                            move |this, _, cx| {
                                if can_prev {
                                    this.set_library_page(page - 1, cx);
                                }
                            },
                        )
                        .when(!can_prev, |view| {
                            view.text_color(rgb(MUTED))
                                .border_color(rgb(BORDER))
                                .cursor_default()
                        }),
                    )
                    .child(
                        icon_button(
                            ("library-page-next", panel_id),
                            "󰒭",
                            "Browse more tracks",
                            cx,
                            move |this, _, cx| {
                                if can_next {
                                    this.set_library_page(page + 1, cx);
                                }
                            },
                        )
                        .when(!can_next, |view| {
                            view.text_color(rgb(MUTED))
                                .border_color(rgb(BORDER))
                                .cursor_default()
                        }),
                    )
            })
            .child(
                icon_button(
                    ("library-filter-favorites", panel_id),
                    "󰓎",
                    "Toggle favorites filter",
                    cx,
                    |this, _, cx| {
                        this.favorites_only = !this.favorites_only;
                        this.refresh_filter(cx);
                    },
                )
                .when(self.favorites_only, |view| {
                    view.text_color(rgb(ACCENT)).border_color(rgb(ACCENT))
                }),
            )
            .child(
                icon_button(
                    ("library-filter-missing", panel_id),
                    "󰌶",
                    "Toggle missing filter",
                    cx,
                    |this, _, cx| {
                        this.missing_only = !this.missing_only;
                        this.refresh_filter(cx);
                    },
                )
                .when(self.missing_only, |view| {
                    view.text_color(rgb(ACCENT)).border_color(rgb(ACCENT))
                }),
            )
            .child(icon_button(
                ("library-add-root", panel_id),
                "󰐕",
                "Add library folder",
                cx,
                |this, _, cx| this.choose_library_root(cx),
            ))
            .child(
                icon_button(
                    ("library-force-scan", panel_id),
                    "󰑐",
                    if self.force_scan {
                        "Force rescans: on"
                    } else {
                        "Force rescans: off"
                    },
                    cx,
                    |this, _, cx| {
                        this.force_scan = !this.force_scan;
                        cx.notify();
                    },
                )
                .when(self.force_scan, |view| {
                    view.text_color(rgb(ACCENT)).border_color(rgb(ACCENT))
                }),
            );
        if !self.selected.is_empty() {
            tools = tools
                .child(icon_button(
                    ("library-enqueue", panel_id),
                    "󰐕",
                    "Append selected tracks to queue",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::Enqueue {
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        )
                    },
                ))
                .child(icon_button(
                    ("library-remove", panel_id),
                    "󰆴",
                    "Remove selected tracks from library",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::RemoveTracks {
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        )
                    },
                ))
                .child(icon_button(
                    ("library-favorite-selected", panel_id),
                    "󰓎",
                    "Add selected to favorites",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::SetFavorite {
                                track_ids: this.panel_selected_tracks(),
                                favorite: true,
                            },
                            cx,
                        )
                    },
                ))
                .child(icon_button(
                    ("library-unfavorite-selected", panel_id),
                    "󰓐",
                    "Remove selected from favorites",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::SetFavorite {
                                track_ids: this.panel_selected_tracks(),
                                favorite: false,
                            },
                            cx,
                        )
                    },
                ));
        }
        let mut panel = panel_surface(("library-panel", panel_id))
            .child(panel_header(
                "Library",
                format!("{total} entries · {} selected", self.selected.len()),
            ))
            .child(self.panel_field(Field::Search, "Search title, artist or album"))
            .child(tools);
        if self.state.system.scan_progress.is_some() || self.state.system.scanning {
            panel = panel.child(self.library_scan_status());
        }
        if self.library_tree_active {
            panel
                .child(self.library_directory_list(panel_id, cx))
                .into_any_element()
        } else {
            let rows = self.library_buffer.rows.len();
            panel
                .child(list_viewport(
                    uniform_list(
                        ("library-rows", panel_id),
                        rows,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .filter_map(|index| {
                                    let row = this.library_buffer.rows.get(index)?.clone();
                                    let id = row.id;
                                    let detail = format!(
                                        "{}{}{} · {}{}",
                                        row.artist,
                                        if row.artist.is_empty() || row.album.is_empty() {
                                            ""
                                        } else {
                                            " / "
                                        },
                                        row.album,
                                        format_time(row.duration.unwrap_or(0.)),
                                        if row.missing { " · File missing" } else { "" }
                                    );
                                    Some(
                                        track_row(
                                            ("library-track", id as u64),
                                            ("library-track-text", id as u64),
                                            this.selected.contains(&id),
                                            row.title,
                                            detail,
                                        )
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, window, cx| {
                                                this.focus_workspace(window, cx);
                                                this.list_focus = Some(ListFocus::Library);
                                                this.select_track(
                                                    id,
                                                    event.modifiers().control
                                                        || event.modifiers().platform,
                                                    cx,
                                                );
                                                if event.click_count() == 2 {
                                                    this.send(Command::Play { track_id: id }, cx);
                                                }
                                            },
                                        )),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .h_full()
                    .min_h_0()
                    .w_full()
                    .track_scroll(&self.library_buffer.scroll),
                ))
                .into_any_element()
        }
    }

    fn selected_queue_ids_in_order(&self) -> Vec<u64> {
        self.state
            .queue
            .entries
            .iter()
            .filter(|entry| self.selected_queue.contains(&entry.id))
            .map(|entry| entry.id)
            .collect()
    }

    fn selected_queue_single(&self) -> Option<u64> {
        if self.selected_queue.len() != 1 {
            return None;
        }
        self.state
            .queue
            .entries
            .iter()
            .find(|entry| self.selected_queue.contains(&entry.id))
            .map(|entry| entry.id)
    }

    fn move_selected_queue(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(queue_id) = self.selected_queue_single() else {
            return;
        };
        let Some(index) = self
            .state
            .queue
            .entries
            .iter()
            .position(|entry| entry.id == queue_id)
        else {
            return;
        };
        let target = if down {
            index.saturating_add(1)
        } else {
            index.saturating_sub(1)
        };
        if target != index && target < self.state.queue.entries.len() {
            self.send(
                Command::MoveQueue {
                    queue_id,
                    index: target,
                },
                cx,
            );
        }
    }

    fn move_queue_entry(&mut self, source_id: u64, target_id: u64, cx: &mut Context<Self>) {
        let Some(source_index) = self
            .state
            .queue
            .entries
            .iter()
            .position(|entry| entry.id == source_id)
        else {
            return;
        };
        let Some(target_index) = self
            .state
            .queue
            .entries
            .iter()
            .position(|entry| entry.id == target_id)
        else {
            return;
        };
        if !self.selected_queue.contains(&source_id) || self.selected_queue.len() == 1 {
            if source_index != target_index {
                self.send(
                    Command::MoveQueue {
                        queue_id: source_id,
                        index: target_index,
                    },
                    cx,
                );
            }
            return;
        }
        if self.selected_queue.contains(&target_id) {
            return;
        }
        let target_after_removal = self.state.queue.entries[..target_index]
            .iter()
            .filter(|entry| !self.selected_queue.contains(&entry.id))
            .count();
        let first_index = self
            .state
            .queue
            .entries
            .iter()
            .position(|entry| self.selected_queue.contains(&entry.id))
            .unwrap_or(source_index);
        let target = target_after_removal + usize::from(target_index > first_index);
        self.send(
            Command::MoveQueueEntries {
                queue_ids: self.selected_queue_ids_in_order(),
                index: target,
            },
            cx,
        );
    }

    pub(super) fn queue_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected_id = self.selected_queue_single();
        let selected_index = selected_id.and_then(|id| {
            self.state
                .queue
                .entries
                .iter()
                .position(|entry| entry.id == id)
        });
        let selection_count = self.selected_queue.len();
        let mut tools = panel_toolbar().min_h(px(32.));
        if !self.state.queue.entries.is_empty() {
            tools = tools.child(icon_button(
                ("queue-save-playlist", panel_id),
                "\u{f0193}",
                "Save queue as playlist",
                cx,
                |this, _, cx| {
                    let track_ids = this
                        .state
                        .queue
                        .entries
                        .iter()
                        .map(|entry| entry.track_id)
                        .collect();
                    let name = format!("Queue {}", Local::now().format("%Y-%m-%d %H-%M-%S"));
                    this.send(Command::CreatePlaylistWithTracks { name, track_ids }, cx);
                },
            ));
        }
        if selection_count > 0 {
            tools = tools
                .child(icon_button(
                    ("queue-play", panel_id),
                    "▶",
                    "Play selected queue entry",
                    cx,
                    |this, _, cx| {
                        let queue_id = this
                            .selected_queue
                            .anchor()
                            .copied()
                            .filter(|id| this.selected_queue.contains(id))
                            .or_else(|| {
                                this.state
                                    .queue
                                    .entries
                                    .iter()
                                    .find(|entry| this.selected_queue.contains(&entry.id))
                                    .map(|entry| entry.id)
                            });
                        if let Some(queue_id) = queue_id {
                            this.send(Command::PlayQueue { queue_id }, cx);
                        }
                    },
                ))
                .child(icon_button(
                    ("queue-remove", panel_id),
                    "×",
                    "Remove selected queue entries",
                    cx,
                    |this, _, cx| {
                        let queue_ids = this.selected_queue_ids_in_order();
                        if !queue_ids.is_empty() {
                            this.selected_queue.clear();
                            this.selected_queue.clear_anchor();
                            this.send(Command::RemoveQueueEntries { queue_ids }, cx);
                            cx.notify();
                        }
                    },
                ));
            if let Some(index) = selected_index {
                if index > 0 {
                    tools = tools.child(icon_button(
                        ("queue-up", panel_id),
                        "↑",
                        "Move selected entry up",
                        cx,
                        |this, _, cx| this.move_selected_queue(false, cx),
                    ));
                }
                if index + 1 < self.state.queue.entries.len() {
                    tools = tools.child(icon_button(
                        ("queue-down", panel_id),
                        "↓",
                        "Move selected entry down",
                        cx,
                        |this, _, cx| this.move_selected_queue(true, cx),
                    ));
                }
            }
        }
        panel_surface(("queue-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                this.resolve_library_drag(&drag.nodes, super::DragDestination::Queue, cx);
            }))
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
            .child(panel_header(
                "Queue",
                format!(
                    "{} queued · {} selected · drag an entry onto its new position",
                    self.state.queue.entries.len(),
                    selection_count,
                ),
            ))
            .when(!self.state.queue.entries.is_empty(), |panel| panel.child(tools))
            .when(self.state.queue.entries.is_empty(), |panel| {
                panel.child(empty_state(
                    "Your queue is empty. Enqueue tracks from Library.",
                ))
            })
            .child(list_viewport(
                uniform_list(
                    ("queue-rows", panel_id),
                    self.state.queue.entries.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let entry = this.state.queue.entries.get(index)?;
                                let id = entry.id;
                                let (title, detail) = this.panel_track_text(entry.track_id);
                                let playing = this.state.queue.current_id == Some(id);
                                let drag = QueueDrag { queue_id: id };
                                Some(
                                    list_row(
                                        ("queue-entry", id),
                                        this.selected_queue.contains(&id),
                                    )
                                    .child(
                                        div()
                                            .w(px(32.0))
                                            .flex_shrink_0()
                                            .text_xs()
                                            .whitespace_nowrap()
                                            .text_color(rgb(if playing { ACCENT } else { MUTED }))
                                            .child(if playing {
                                                "▶".to_owned()
                                            } else {
                                                (index + 1).to_string()
                                            }),
                                    )
                                    .child(row_text(("queue-entry-text", id), title, detail))
                                    .on_mouse_down(
                                        gpui::MouseButton::Right,
                                        cx.listener(move |this, _, window, cx| {
                                            this.focus_workspace(window, cx);
                                            this.list_focus = Some(ListFocus::Queue);
                                            if !this.selected_queue.contains(&id) {
                                                this.selected_queue.clear();
                                                this.selected_queue.insert(id);
                                                this.selected_queue.set_anchor(id);
                                                cx.notify();
                                            }
                                        }),
                                    )
                                    .on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, window, cx| {
                                            this.focus_workspace(window, cx);
                                            this.list_focus = Some(ListFocus::Queue);
                                            let modifiers = event.modifiers();
                                            this.select_queue_entry(
                                                id,
                                                modifiers.shift,
                                                modifiers.control || modifiers.platform,
                                                cx,
                                            );
                                            if event.click_count() == 2 {
                                                this.send(Command::PlayQueue { queue_id: id }, cx);
                                            }
                                        },
                                    ))
                                    .on_drag(drag, |drag, _, _, cx| cx.new(|_| *drag))
                                    .on_drop(cx.listener(
                                        move |this, drag: &QueueDrag, window, cx| {
                                            window.prevent_default();
                                            this.move_queue_entry(drag.queue_id, id, cx);
                                        },
                                    ))
                                    .drag_over::<QueueDrag>(|style, _, _, _| {
                                        style.bg(rgb(HIGHLIGHT))
                                    }),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                    window.prevent_default();
                    this.resolve_library_drag(&drag.nodes, super::DragDestination::Queue, cx);
                }))
                .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
                .h_full()
                .min_h_0()
                .w_full(),
            ))
            .into_any_element()
    }

    fn move_selected_playlist_entry(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(entry_id) = self.selected_entry else {
            return;
        };
        let Some(index) = self
            .playlist_entries
            .rows
            .iter()
            .position(|entry| entry.id == entry_id)
        else {
            return;
        };
        let target = if down {
            index.saturating_add(1)
        } else {
            index.saturating_sub(1)
        };
        if target != index && target < self.playlist_entries.total {
            self.send(
                Command::MovePlaylistEntry {
                    entry_id,
                    index: self.playlist_entries.offset + target,
                },
                cx,
            );
        }
    }

    pub(super) fn playlists_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let playlist_id = self.selected_playlist;
        let selected_name = playlist_id
            .and_then(|id| {
                self.playlist_buffer
                    .rows
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_default();
        let entry_count = if playlist_id == self.playlist_entries.playlist_id {
            self.playlist_entries.total
        } else {
            0
        };
        let page_size = self.view_page_size();
        let page = self.playlist_buffer.offset / page_size;
        let page_count = self.playlist_buffer.total.div_ceil(page_size).max(1);
        let entry_page = self.playlist_entries.offset / page_size;
        let entry_page_count = entry_count.div_ceil(page_size).max(1);
        let mut panel = panel_surface(("playlists-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                if let Some(id) = this.selected_playlist {
                    this.resolve_library_drag(
                        &drag.nodes,
                        super::DragDestination::Playlist(id),
                        cx,
                    );
                } else {
                    this.panel_error("Select a playlist before dropping tracks.", cx);
                }
            }))
            .child(panel_header(
                "Playlists",
                format!(
                    "{} playlists · select one to manage its tracks",
                    self.playlist_buffer.total
                ),
            ))
            .child(self.panel_field(Field::PlaylistName, "Playlist name"))
            .child(
                panel_toolbar()
                    .child(icon_button(
                        ("playlist-create", panel_id),
                        "+",
                        "Create playlist",
                        cx,
                        |this, _, cx| {
                            let name = this.value(Field::PlaylistName, cx).trim().to_owned();
                            if name.is_empty() {
                                this.panel_error("Enter a playlist name first.", cx);
                            } else {
                                this.send(Command::CreatePlaylist { name }, cx);
                            }
                        },
                    ))
                    .child(icon_button(
                        ("playlist-import", panel_id),
                        "↓",
                        "Import M3U",
                        cx,
                        |this, _, cx| this.choose_playlist_import(cx),
                    ))
                    .when(page > 0, |tools| {
                        tools.child(icon_button(
                            ("playlist-page-prev", panel_id),
                            "󰒮",
                            "Browse earlier playlists",
                            cx,
                            move |this, _, cx| {
                                this.request_playlist_summaries(
                                    this.playlist_buffer.offset.saturating_sub(page_size),
                                    cx,
                                )
                            },
                        ))
                    })
                    .when(page + 1 < page_count, |tools| {
                        tools.child(icon_button(
                            ("playlist-page-next", panel_id),
                            "󰒭",
                            "Browse more playlists",
                            cx,
                            move |this, _, cx| {
                                this.request_playlist_summaries(
                                    this.playlist_buffer.offset.saturating_add(page_size),
                                    cx,
                                )
                            },
                        ))
                    }),
            )
            .child(list_viewport(
                uniform_list(
                    ("playlist-catalog", panel_id),
                    self.playlist_buffer.rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let p = this.playlist_buffer.rows.get(index)?.clone();
                                let id = p.id;
                                Some(
                                    list_row(
                                        ("playlist", id as u64),
                                        this.selected_playlist == Some(id),
                                    )
                                    .child(row_text(
                                        ("playlist-text", id as u64),
                                        p.name,
                                        format!("{} tracks", p.entry_count),
                                    ))
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.selected_playlist = Some(id);
                                            this.selected_entry = None;
                                            this.clear_metadata_selection(cx);
                                            this.request_playlist_entries(id, 0, cx);
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(px(160.))
                .w_full(),
            ));
        if let Some(id) = playlist_id {
            panel = panel
                .child(caption(format!("{} · {entry_count} tracks", selected_name)).truncate())
                .child(
                    panel_toolbar()
                        .child(icon_button(
                            ("playlist-add", panel_id),
                            "+",
                            "Add selected tracks",
                            cx,
                            move |this, _, cx| {
                                this.send(
                                    Command::AddPlaylist {
                                        playlist_id: id,
                                        track_ids: this.panel_selected_tracks(),
                                    },
                                    cx,
                                )
                            },
                        ))
                        .child(icon_button(
                            ("playlist-play", panel_id),
                            "▶",
                            "Play playlist",
                            cx,
                            move |this, _, cx| {
                                this.send(Command::PlayPlaylist { playlist_id: id }, cx)
                            },
                        ))
                        .child(icon_button(
                            ("playlist-export", panel_id),
                            "↑",
                            "Export playlist",
                            cx,
                            move |this, _, cx| this.choose_playlist_export(id, cx),
                        ))
                        .child(icon_button(
                            ("playlist-rename", panel_id),
                            "✎",
                            "Rename playlist",
                            cx,
                            move |this, _, cx| {
                                let name = this.value(Field::PlaylistName, cx).trim().to_owned();
                                if name.is_empty() {
                                    this.panel_error("Enter a playlist name first.", cx);
                                } else {
                                    this.send(
                                        Command::RenamePlaylist {
                                            playlist_id: id,
                                            name,
                                        },
                                        cx,
                                    );
                                }
                            },
                        ))
                        .child(icon_button(
                            ("playlist-delete", panel_id),
                            "×",
                            "Delete playlist",
                            cx,
                            move |this, _, cx| {
                                this.selected_playlist = None;
                                this.selected_entry = None;
                                this.send(Command::DeletePlaylist { playlist_id: id }, cx);
                            },
                        ))
                        .when(self.selected_entry.is_some(), |bar| {
                            bar.child(icon_button(
                                ("entry-play", panel_id),
                                "▶",
                                "Play selected track",
                                cx,
                                move |this, _, cx| {
                                    if let Some(entry) = this
                                        .playlist_entries
                                        .rows
                                        .iter()
                                        .find(|entry| Some(entry.id) == this.selected_entry)
                                    {
                                        this.send(
                                            Command::Play {
                                                track_id: entry.track_id,
                                            },
                                            cx,
                                        );
                                    }
                                },
                            ))
                            .child(icon_button(
                                ("entry-remove", panel_id),
                                "×",
                                "Remove selected entry",
                                cx,
                                move |this, _, cx| {
                                    if let Some(entry_id) = this.selected_entry.take() {
                                        this.send(Command::RemovePlaylistEntry { entry_id }, cx);
                                        this.clear_metadata_selection(cx);
                                    }
                                },
                            ))
                            .child(icon_button(
                                ("entry-up", panel_id),
                                "↑",
                                "Move entry up",
                                cx,
                                move |this, _, cx| this.move_selected_playlist_entry(false, cx),
                            ))
                            .child(icon_button(
                                ("entry-down", panel_id),
                                "↓",
                                "Move entry down",
                                cx,
                                move |this, _, cx| this.move_selected_playlist_entry(true, cx),
                            ))
                        })
                        .when(entry_page > 0, |tools| {
                            tools.child(icon_button(
                                ("playlist-entry-prev", panel_id),
                                "󰒮",
                                "Browse earlier tracks",
                                cx,
                                move |this, _, cx| {
                                    this.request_playlist_entries(
                                        id,
                                        this.playlist_entries.offset.saturating_sub(page_size),
                                        cx,
                                    )
                                },
                            ))
                        })
                        .when(entry_page + 1 < entry_page_count, |tools| {
                            tools.child(icon_button(
                                ("playlist-entry-next", panel_id),
                                "󰒭",
                                "Browse more tracks",
                                cx,
                                move |this, _, cx| {
                                    this.request_playlist_entries(
                                        id,
                                        this.playlist_entries.offset.saturating_add(page_size),
                                        cx,
                                    )
                                },
                            ))
                        }),
                )
                .child(list_viewport(
                    uniform_list(
                        ("playlist-entries", panel_id),
                        self.playlist_entries.rows.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .filter_map(|index| {
                                    let e = this.playlist_entries.rows.get(index)?.clone();
                                    let id = e.id;
                                    let track_id = e.track_id;
                                    Some(
                                        list_row(
                                            ("playlist-entry", id as u64),
                                            this.selected_entry == Some(id),
                                        )
                                        .child(
                                            caption(
                                                (this.playlist_entries.offset + index + 1)
                                                    .to_string(),
                                            )
                                            .w(px(28.)),
                                        )
                                        .child(row_text(
                                            ("playlist-entry-text", id as u64),
                                            e.title,
                                            format!(
                                                "{}{}{}",
                                                e.artist,
                                                if e.artist.is_empty() || e.album.is_empty() {
                                                    ""
                                                } else {
                                                    " / "
                                                },
                                                e.album
                                            ),
                                        ))
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, window, cx| {
                                                this.focus_workspace(window, cx);
                                                this.selected_entry = Some(id);
                                                this.update_metadata_track(track_id, cx);
                                                if event.click_count() == 2 {
                                                    this.send(Command::Play { track_id }, cx);
                                                }
                                                cx.notify();
                                            },
                                        )),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .h_full()
                    .min_h_0()
                    .w_full(),
                ));
        }
        panel.into_any_element()
    }

    pub(super) fn metadata_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut panel = panel_surface(("metadata-panel", panel_id));
        if let Some(track) = self.full_track.as_ref() {
            let id = track.id;
            let favorite = track.favorite;
            let metadata_dirty = self.value(Field::Title, cx) != track.title
                || self.value(Field::Artist, cx) != track.artist
                || self.value(Field::Album, cx) != track.album;
            let file_path = if track.missing {
                "Missing".to_owned()
            } else {
                track.path.to_string_lossy().into_owned()
            };
            let last_played = self.last_played_text(id, track.last_played);
            let last_played = last_played
                .strip_prefix("Last played · ")
                .unwrap_or(&last_played)
                .to_owned();
            let details = div()
                .id(("metadata-details", panel_id))
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(UI_INSET))
                .gap_3()
                .child(
                    row()
                        .justify_between()
                        .items_center()
                        .child(caption(format!("Track #{id}")).text_lg().text_color(rgb(TEXT)))
                        .child(icon_button(
                            ("metadata-favorite", panel_id),
                            if favorite { "󰓎" } else { "󰓐" },
                            if favorite {
                                "Remove from favorites"
                            } else {
                                "Add to favorites"
                            },
                            cx,
                            move |this, _, cx| {
                                this.send(
                                    Command::SetFavorite {
                                        track_ids: vec![id],
                                        favorite: !favorite,
                                    },
                                    cx,
                                );
                            },
                        )),
                )
                .child(metadata_group(
                    "File",
                    metadata_row(
                        metadata_pair("File path", file_path),
                        metadata_pair("Availability", if track.missing {
                            "Missing"
                        } else {
                            "Available"
                        }),
                    ),
                ))
                .child(metadata_group(
                    "Technical details",
                    column()
                        .gap_1()
                        .child(metadata_row(
                            metadata_pair(
                                "Codec",
                                if track.codec.is_empty() {
                                    "—".to_owned()
                                } else {
                                    track.codec.clone()
                                },
                            ),
                            metadata_pair(
                                "Sample rate",
                                if track.sample_rate == 0 {
                                    "—".to_owned()
                                } else {
                                    format!("{} Hz", track.sample_rate)
                                },
                            ),
                        ))
                        .child(metadata_row(
                            metadata_pair(
                                "Channels",
                                if track.channels == 0 {
                                    "—".to_owned()
                                } else {
                                    track.channels.to_string()
                                },
                            ),
                            metadata_pair("Duration", track.duration.map_or("—".into(), format_time)),
                        ))
                        .child(metadata_row(
                            metadata_pair(
                                "Bitrate",
                                track.bitrate_bps.map_or("—".into(), |value| {
                                    format!("{:.1} kbps", value as f64 / 1000.0)
                                }),
                            ),
                            metadata_pair(
                                "Bits/sample",
                                track
                                    .bits_per_sample
                                    .map_or("—".into(), |value| value.to_string()),
                            ),
                        ))
                        .child(metadata_row(
                            metadata_pair(
                                "Disc",
                                track.disc_number.map_or("—".into(), |value| value.to_string()),
                            ),
                            metadata_pair(
                                "Track",
                                track.track_number.map_or("—".into(), |value| value.to_string()),
                            ),
                        ))
                        .child(metadata_row(
                            metadata_pair("Release", track.release_date.as_deref().unwrap_or("—")),
                            metadata_pair("Plays", track.play_count.to_string()),
                        ))
                        .child(metadata_row(
                            metadata_pair("Last played", last_played),
                            metadata_pair("", ""),
                        ))
                        .when_some(track.cue.as_ref(), |details, cue| {
                            details.child(caption(format!(
                                "CUE: {} · Track {} · {}–{}",
                                cue.sheet.display(),
                                cue.number,
                                format_time(cue.start_seconds()),
                                cue.end_seconds().map_or("—".into(), format_time)
                            )))
                        }),
                ))
                .child(metadata_group(
                    "Tags",
                    column()
                        .gap_2()
                        .child(self.panel_field(Field::Title, "Title"))
                        .child(self.panel_field(Field::Artist, "Artist"))
                        .child(self.panel_field(Field::Album, "Album"))
                        .child(
                            row()
                                .gap_2()
                                .child(
                                    icon_button(
                                        ("metadata-save", panel_id),
                                        "󰆓",
                                        "Save metadata",
                                        cx,
                                        move |this, _, cx| {
                                            if metadata_dirty {
                                                this.send(
                                                    Command::EditTrack {
                                                        track_id: id,
                                                        title: this.value(Field::Title, cx),
                                                        artist: this.value(Field::Artist, cx),
                                                        album: this.value(Field::Album, cx),
                                                    },
                                                    cx,
                                                );
                                            }
                                        },
                                    )
                                    .when(!metadata_dirty, |view| {
                                        view.opacity(0.4).cursor_default()
                                    }),
                                )
                                .child(caption("Media files are never modified.")),
                        ),
                ));
            panel = panel.child(details);
        } else {
            panel = panel.child(column().flex_1().p(px(UI_INSET)).gap_2().child(empty_state(
                "Start playback or select a Library track to view its details.",
            )));
        }
        panel.into_any_element()
    }
    fn set_history_sort(&mut self, sort: HistorySort, cx: &mut Context<Self>) {
        if self.history_sort == sort {
            self.history_sort_desc = !self.history_sort_desc;
        } else {
            self.history_sort = sort;
            self.history_sort_desc = matches!(sort, HistorySort::Recent | HistorySort::Plays);
        }
        cx.notify();
    }

    fn history_sort_button(
        &self,
        panel_id: u64,
        label: &'static str,
        sort: HistorySort,
        width: Option<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.history_sort == sort;
        let marker = if active {
            if self.history_sort_desc { " ↓" } else { " ↑" }
        } else {
            ""
        };
        let id = match sort {
            HistorySort::Recent => ("history-sort-recent", panel_id),
            HistorySort::Title => ("history-sort-title", panel_id),
            HistorySort::Plays => ("history-sort-plays", panel_id),
        };
        let mut control = button(
            id,
            format!("{label}{marker}"),
            cx,
            move |this, _, cx| this.set_history_sort(sort, cx),
        );
        if let Some(width) = width {
            control = control.w(width).flex_shrink_0();
        } else {
            control = control.flex_1().min_w_0();
        }
        control
    }

    pub(super) fn history_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut history = self.state.library.history.as_ref().clone();
        let sort = self.history_sort;
        let descending = self.history_sort_desc;
        history.sort_by(|a, b| {
            let ordering = match sort {
                HistorySort::Recent => a.played_at.cmp(&b.played_at),
                HistorySort::Title => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
                HistorySort::Plays => a.play_count.cmp(&b.play_count),
            };
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        let history_len = history.len();
        let header = row()
            .items_center()
            .gap_2()
            .px_2()
            .h(gpui::rems(2.25))
            .flex_shrink_0()
            .child(caption("#").w(px(32.)).flex_shrink_0())
            .child(self.history_sort_button(panel_id, "Track", HistorySort::Title, None, cx))
            .child(self.history_sort_button(
                panel_id,
                "Plays",
                HistorySort::Plays,
                Some(px(84.)),
                cx,
            ))
            .child(self.history_sort_button(
                panel_id,
                "Last played",
                HistorySort::Recent,
                Some(px(148.)),
                cx,
            ));
        panel_surface(("history-panel", panel_id))
            .child(panel_header(
                "Recent tracks",
                format!("{} tracks · click a column to sort", history_len),
            ))
            .child(header)
            .child(list_viewport(
                uniform_list(
                    ("history-rows", panel_id),
                    history_len,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let item = history.get(index)?;
                                let id = item.track_id;
                                let title = if item.title.is_empty() {
                                    "Untitled".to_owned()
                                } else {
                                    item.title.clone()
                                };
                                Some(
                                    list_row(
                                        ("history-entry", id as u64),
                                        this.selected.contains(&id),
                                    )
                                    .child(
                                        caption((index + 1).to_string())
                                            .w(px(32.))
                                            .flex_shrink_0(),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_sm()
                                            .truncate()
                                            .child(title),
                                    )
                                    .child(
                                        caption(item.play_count.to_string())
                                            .w(px(84.))
                                            .flex_shrink_0()
                                            .truncate(),
                                    )
                                    .child(
                                        caption(this.last_played_text(id, Some(item.played_at)))
                                            .w(px(148.))
                                            .flex_shrink_0()
                                            .truncate(),
                                    )
                                    .on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, _, cx| {
                                            this.select_track(id, false, cx);
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id: id }, cx);
                                            }
                                        },
                                    )),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h_full()
                .min_h_0()
                .w_full(),
            ))
            .into_any_element()
    }
}
