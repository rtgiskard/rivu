use super::{
    ACCENT, BORDER, Dragging, ERROR, ERROR_BG, Field, GuiApp, HIGHLIGHT, ListFocus, MUTED,
    Measured, QueueDrag, UI_INSET, button, column, format_time, icon_button, library::LibraryDrag,
    row, row_text,
};
pub(super) use super::{TRACK_HEIGHT, caption, list_row};
use crate::model::{Command, PlaybackStatus, RepeatMode};
use gpui::{AnyElement, Context, Div, Window, div, prelude::*, px, rgb, uniform_list};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn last_played_text(played_at: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64;
    let Some(age) = now.checked_sub(played_at).filter(|age| *age >= 0) else {
        return format!("Last played · Unix {played_at}");
    };
    let elapsed = if age < 60 {
        format!("{age}s")
    } else if age < 3600 {
        format!("{}m", age / 60)
    } else if age < 86400 {
        format!("{}h", age / 3600)
    } else {
        format!("{}d", age / 86400)
    };
    format!("Last played {elapsed} ago · Unix {played_at}")
}

impl GuiApp {
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
        let status = match self.state.status {
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
            PlaybackStatus::Stopped => "Stopped",
        };
        let duration = self
            .state
            .duration
            .or_else(|| track.and_then(|track| track.duration));
        let preview_position = self.seek_preview.unwrap_or(self.state.position);
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
            self.state.volume,
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
                    if self.state.shuffle {
                        "Shuffle: on (s)"
                    } else {
                        "Shuffle: off (s)"
                    },
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::Shuffle {
                                enabled: !this.state.shuffle,
                            },
                            cx,
                        )
                    },
                )
                .size(gpui::rems(3.))
                .text_2xl()
                .when(self.state.shuffle, |view| {
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
                    if self.state.status == PlaybackStatus::Playing {
                        "󰏤"
                    } else {
                        "󰐊"
                    },
                    if self.state.status == PlaybackStatus::Playing {
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
                    if self.state.repeat == RepeatMode::One {
                        "󰑘"
                    } else {
                        "󰑖"
                    },
                    match self.state.repeat {
                        RepeatMode::Off => "Repeat: off (r)",
                        RepeatMode::All => "Repeat: all (r)",
                        RepeatMode::One => "Repeat: one (r)",
                    },
                    cx,
                    |this, _, cx| {
                        let mode = match this.state.repeat {
                            RepeatMode::Off => RepeatMode::All,
                            RepeatMode::All => RepeatMode::One,
                            RepeatMode::One => RepeatMode::Off,
                        };
                        this.send(Command::Repeat { mode }, cx);
                    },
                )
                .size(gpui::rems(3.))
                .text_2xl()
                .when(self.state.repeat != RepeatMode::Off, |view| {
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
                    .child(if self.state.config.nerd_symbols {
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
                caption(format!("{:.0}%", self.state.volume * 100.))
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
                                            .child(if self.state.config.nerd_symbols {
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
        self.state
            .library
            .iter()
            .filter(|track| self.selected.contains(&track.id))
            .map(|track| track.id)
            .collect()
    }

    pub(super) fn panel_track_text(&self, track_id: i64) -> (String, String) {
        match self
            .library_index
            .get(&track_id)
            .and_then(|index| self.state.library.get(*index))
        {
            Some(track) => {
                let detail = format!(
                    "{}{}{}  ·  {}{}{}",
                    track.artist,
                    if track.artist.is_empty() || track.album.is_empty() {
                        ""
                    } else {
                        " / "
                    },
                    track.album,
                    format_time(track.duration.unwrap_or(0.0)),
                    track.bitrate_bps.map_or(String::new(), |bitrate| format!(
                        "  ·  {:.1} kbps",
                        bitrate as f64 / 1000.0
                    )),
                    if track.missing {
                        "  ·  File missing"
                    } else {
                        ""
                    }
                );
                (track.title.clone(), detail)
            }
            None => ("Missing track".into(), format!("Track #{track_id}")),
        }
    }

    fn panel_error(&mut self, message: &'static str, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    pub(super) fn library_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.filtered_rows.len();
        let mut tools = row().flex_wrap().flex_shrink_0().child(button(
            ("scan", panel_id),
            "Scan paths",
            cx,
            |this, _, cx| {
                let path = this.value(Field::Path, cx);
                let paths = if path.trim().is_empty() {
                    this.state.config.library_roots.clone()
                } else {
                    path.lines()
                        .map(str::trim)
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from)
                        .collect()
                };
                this.send(
                    Command::Scan {
                        paths,
                        force: this.force_scan,
                    },
                    cx,
                );
            },
        ));
        tools = tools
            .child(button(
                ("library-force-scan", panel_id),
                if self.force_scan {
                    "Force metadata rescan: on"
                } else {
                    "Force metadata rescan: off"
                },
                cx,
                |this, _, cx| {
                    this.force_scan = !this.force_scan;
                    cx.notify();
                },
            ))
            .child(button(
                ("library-filter-favorites", panel_id),
                if self.favorites_only {
                    "Favorites only: on"
                } else {
                    "Favorites only: off"
                },
                cx,
                |this, _, cx| {
                    this.favorites_only = !this.favorites_only;
                    this.refresh_filter(cx);
                    cx.notify();
                },
            ))
            .child(button(
                ("library-filter-missing", panel_id),
                if self.missing_only {
                    "Missing only: on"
                } else {
                    "Missing only: off"
                },
                cx,
                |this, _, cx| {
                    this.missing_only = !this.missing_only;
                    this.refresh_filter(cx);
                    cx.notify();
                },
            ));
        if !self.selected.is_empty() {
            tools = tools
                .child(button(
                    ("library-enqueue", panel_id),
                    "Append selected to queue",
                    cx,
                    |this, _, cx| {
                        let track_ids = this.panel_selected_tracks();
                        this.send(Command::Enqueue { track_ids }, cx);
                    },
                ))
                .child(button(
                    ("library-remove", panel_id),
                    "Remove selected from library (keep files)",
                    cx,
                    |this, _, cx| {
                        let track_ids = this.panel_selected_tracks();
                        this.send(Command::RemoveTracks { track_ids }, cx);
                    },
                ));
            tools = tools
                .child(button(
                    ("library-favorite-selected", panel_id),
                    "Favorite selected",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::SetFavorite {
                                track_ids: this.panel_selected_tracks(),
                                favorite: true,
                            },
                            cx,
                        );
                    },
                ))
                .child(button(
                    ("library-unfavorite-selected", panel_id),
                    "Unfavorite selected",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::SetFavorite {
                                track_ids: this.panel_selected_tracks(),
                                favorite: false,
                            },
                            cx,
                        );
                    },
                ));
        }
        let mut panel = column()
            .id(("library-panel", panel_id))
            .size_full()
            .child(self.panel_field(Field::Search, "Search title, artist or album"))
            .child(self.panel_field(
                Field::Path,
                "File or folder path · blank scans configured roots",
            ))
            .child(tools)
            .child(caption(format!(
                "{count} tracks · {} selected · Ctrl-click to select multiple",
                self.selected.len()
            )));
        if self.state.scanning {
            panel = panel.child(caption(self.state.scan_message.clone()).truncate());
        }
        if count == 0 {
            panel = panel.child(caption(
                "No matching tracks. Scan a music file or folder to get started.",
            ));
        }
        if self.library_tree_active {
            return panel
                .child(caption(
                    "↑/↓ navigate · ←/→ collapse/expand · Space toggles folders · Enter plays",
                ))
                .child(self.library_tree_list(panel_id, cx))
                .into_any_element();
        }
        panel
            .child(
                uniform_list(
                    ("library-rows", panel_id),
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let track =
                                    this.state.library.get(*this.filtered_rows.get(index)?)?;
                                let id = track.id;
                                let (title, detail) = this.panel_track_text(id);
                                Some(
                                    list_row(
                                        ("library-track", id as u64),
                                        this.selected.contains(&id),
                                    )
                                    .child(row_text(title, detail))
                                    .on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, window, cx| {
                                            this.focus_workspace(window, cx);
                                            this.list_focus = Some(ListFocus::Library);
                                            let modifiers = event.modifiers();
                                            this.select_track(
                                                id,
                                                modifiers.control || modifiers.platform,
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
                .flex_1()
                .min_h_0()
                .w_full(),
            )
            .into_any_element()
    }

    fn selected_queue_ids_in_order(&self) -> Vec<u64> {
        self.state
            .queue
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
        if target != index && target < self.state.queue.len() {
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
            .iter()
            .position(|entry| entry.id == source_id)
        else {
            return;
        };
        let Some(target_index) = self
            .state
            .queue
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
        let target_after_removal = self.state.queue[..target_index]
            .iter()
            .filter(|entry| !self.selected_queue.contains(&entry.id))
            .count();
        let first_index = self
            .state
            .queue
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
        let selected_index =
            selected_id.and_then(|id| self.state.queue.iter().position(|entry| entry.id == id));
        let selection_count = self.selected_queue.len();
        let mut tools = row().min_h(px(32.)).flex_wrap().flex_shrink_0();
        if selection_count > 0 {
            tools = tools
                .child(icon_button(
                    ("queue-play", panel_id),
                    "▶",
                    "Play selected queue entry",
                    cx,
                    |this, _, cx| {
                        let queue_id = this
                            .queue_anchor
                            .filter(|id| this.selected_queue.contains(id))
                            .or_else(|| {
                                this.state
                                    .queue
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
                            this.queue_anchor = None;
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
                if index + 1 < self.state.queue.len() {
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
        column()
            .id(("queue-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                let track_ids = this.library_drag_track_ids(&drag.node);
                if !track_ids.is_empty() {
                    this.send(Command::Enqueue { track_ids }, cx);
                }
            }))
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
            .child(caption(format!(
                "{} queued · {} selected · drag an entry onto its new position",
                self.state.queue.len(),
                selection_count,
            )))
            .when(selection_count > 0, |panel| panel.child(tools))
            .when(self.state.queue.is_empty(), |panel| {
                panel.child(caption("Your queue is empty. Enqueue tracks from Library."))
            })
            .child(
                uniform_list(
                    ("queue-rows", panel_id),
                    self.state.queue.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let entry = this.state.queue.get(index)?;
                                let id = entry.id;
                                let (title, detail) = this.panel_track_text(entry.track_id);
                                let playing = this.state.current_queue_id == Some(id);
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
                                    .child(row_text(title, detail))
                                    .on_mouse_down(
                                        gpui::MouseButton::Right,
                                        cx.listener(move |this, _, window, cx| {
                                            this.focus_workspace(window, cx);
                                            this.list_focus = Some(ListFocus::Queue);
                                            if !this.selected_queue.contains(&id) {
                                                this.selected_queue.clear();
                                                this.selected_queue.insert(id);
                                                this.queue_anchor = Some(id);
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
                .flex_1()
                .min_h_0()
                .w_full(),
            )
            .into_any_element()
    }

    fn move_selected_playlist_entry(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(entry_id) = self.selected_entry else {
            return;
        };
        let Some(playlist) = self
            .state
            .playlists
            .iter()
            .find(|playlist| Some(playlist.id) == self.selected_playlist)
        else {
            return;
        };
        let Some(index) = playlist
            .entries
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
        if target != index && target < playlist.entries.len() {
            self.send(
                Command::MovePlaylistEntry {
                    entry_id,
                    index: target,
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
        let selected = self.selected_playlist.and_then(|id| {
            self.state
                .playlists
                .iter()
                .find(|playlist| playlist.id == id)
        });
        let playlist_id = selected.map(|playlist| playlist.id);
        let entry_count = selected.map_or(0, |playlist| playlist.entries.len());
        let selected_index = selected.and_then(|playlist| {
            playlist
                .entries
                .iter()
                .position(|entry| Some(entry.id) == self.selected_entry)
        });
        let selected_name = selected.map(|playlist| playlist.name.clone());
        let mut manage = row().flex_wrap().flex_shrink_0().child(icon_button(
            ("playlist-create", panel_id),
            "+",
            "Create playlist",
            cx,
            |this, _, cx| {
                let name = this.value(Field::PlaylistName, cx).trim().to_owned();
                if name.is_empty() {
                    this.panel_error("Enter a playlist name first.", cx);
                    return;
                }
                this.send(Command::CreatePlaylist { name }, cx);
            },
        ));
        if let Some(playlist_id) = playlist_id {
            manage = manage
                .child(icon_button(
                    ("playlist-rename", panel_id),
                    "✎",
                    "Rename playlist",
                    cx,
                    move |this, _, cx| {
                        let name = this.value(Field::PlaylistName, cx).trim().to_owned();
                        if name.is_empty() {
                            this.panel_error("Enter a playlist name first.", cx);
                            return;
                        }
                        this.send(Command::RenamePlaylist { playlist_id, name }, cx);
                    },
                ))
                .child(icon_button(
                    ("playlist-delete", panel_id),
                    "×",
                    "Delete playlist",
                    cx,
                    move |this, _, cx| {
                        if entry_count > 0 {
                            this.playlist_delete_confirm = Some(playlist_id);
                        } else {
                            this.selected_playlist = None;
                            this.selected_entry = None;
                            this.send(Command::DeletePlaylist { playlist_id }, cx);
                        }
                        cx.notify();
                    },
                ));
        }
        let mut files = row().flex_wrap().flex_shrink_0().child(icon_button(
            ("playlist-import", panel_id),
            "↓",
            "Choose M3U to import",
            cx,
            |this, _, cx| this.choose_playlist_import(cx),
        ));
        if let Some(playlist_id) = playlist_id {
            files = files.child(icon_button(
                ("playlist-export", panel_id),
                "↑",
                "Choose destination for M3U export",
                cx,
                move |this, _, cx| this.choose_playlist_export(playlist_id, cx),
            ));
        }
        let catalog_height = (self.state.playlists.len().max(1) as f32 * TRACK_HEIGHT).min(126.0);
        let mut panel = column()
            .id(("playlists-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                let Some(playlist_id) = this.selected_playlist else {
                    this.panel_error("Select a playlist before dropping tracks.", cx);
                    return;
                };
                let track_ids = this.library_drag_track_ids(&drag.node);
                if !track_ids.is_empty() {
                    this.send(
                        Command::AddPlaylist {
                            playlist_id,
                            track_ids,
                        },
                        cx,
                    );
                }
            }))
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
            .child(self.panel_field(Field::PlaylistName, "Playlist name"))
            .child(manage)
            .child(files)
            .child(
                uniform_list(
                    ("playlist-catalog", panel_id),
                    self.state.playlists.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let playlist = this.state.playlists.get(index)?;
                                let id = playlist.id;
                                Some(
                                    list_row(
                                        ("playlist", id as u64),
                                        this.selected_playlist == Some(id),
                                    )
                                    .child(row_text(
                                        playlist.name.clone(),
                                        format!("{} tracks", playlist.entries.len()),
                                    ))
                                    .on_click(cx.listener(
                                        move |this, _: &gpui::ClickEvent, window, cx| {
                                            this.focus_workspace(window, cx);
                                            this.selected_playlist = Some(id);
                                            this.playlist_delete_confirm = None;
                                            this.selected_entry = None;
                                            if let Some(name) = this
                                                .state
                                                .playlists
                                                .iter()
                                                .find(|playlist| playlist.id == id)
                                                .map(|playlist| playlist.name.clone())
                                            {
                                                this.set_value(Field::PlaylistName, name, cx);
                                            }
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(px(catalog_height))
                .flex_shrink_0()
                .w_full(),
            );
        if let Some(playlist_id) = playlist_id {
            let mut tools = row().flex_wrap().flex_shrink_0();
            if entry_count > 0 {
                tools = tools.child(icon_button(
                    ("playlist-play", panel_id),
                    "▷",
                    "Play playlist",
                    cx,
                    move |this, _, cx| {
                        this.send(Command::PlayPlaylist { playlist_id }, cx);
                    },
                ));
            }
            if !self.selected.is_empty() {
                tools = tools.child(icon_button(
                    ("playlist-add", panel_id),
                    "+",
                    "Add selected tracks",
                    cx,
                    move |this, _, cx| {
                        this.send(
                            Command::AddPlaylist {
                                playlist_id,
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        );
                    },
                ));
            }
            if let Some(index) = selected_index {
                tools = tools
                    .child(icon_button(
                        ("entry-play", panel_id),
                        "▶",
                        "Play selected track",
                        cx,
                        |this, _, cx| {
                            let track_id = this
                                .state
                                .playlists
                                .iter()
                                .find(|playlist| Some(playlist.id) == this.selected_playlist)
                                .and_then(|playlist| {
                                    playlist
                                        .entries
                                        .iter()
                                        .find(|entry| Some(entry.id) == this.selected_entry)
                                })
                                .map(|entry| entry.track_id);
                            if let Some(track_id) = track_id {
                                this.send(Command::Play { track_id }, cx);
                            }
                        },
                    ))
                    .child(icon_button(
                        ("entry-remove", panel_id),
                        "×",
                        "Remove selected entry",
                        cx,
                        |this, _, cx| {
                            if let Some(entry_id) = this.selected_entry.take() {
                                this.send(Command::RemovePlaylistEntry { entry_id }, cx);
                                cx.notify();
                            }
                        },
                    ));
                if index > 0 {
                    tools = tools.child(icon_button(
                        ("entry-up", panel_id),
                        "↑",
                        "Move selected entry up",
                        cx,
                        |this, _, cx| this.move_selected_playlist_entry(false, cx),
                    ));
                }
                if index + 1 < entry_count {
                    tools = tools.child(icon_button(
                        ("entry-down", panel_id),
                        "↓",
                        "Move selected entry down",
                        cx,
                        |this, _, cx| this.move_selected_playlist_entry(true, cx),
                    ));
                }
            }
            panel = panel
                .child(
                    caption(format!(
                        "{} · {entry_count} tracks",
                        selected_name.unwrap_or_default()
                    ))
                    .truncate(),
                )
                .child(tools)
                .when(entry_count == 0, |panel| {
                    panel.child(caption(
                        "Select tracks in Library, then choose Add selected.",
                    ))
                })
                .child(
                    uniform_list(
                        ("playlist-entries", panel_id),
                        entry_count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            let Some(playlist) = this
                                .state
                                .playlists
                                .iter()
                                .find(|playlist| playlist.id == playlist_id)
                            else {
                                return Vec::new();
                            };
                            range
                                .filter_map(|index| {
                                    let entry = playlist.entries.get(index)?;
                                    let id = entry.id;
                                    let track_id = entry.track_id;
                                    let (title, detail) = this.panel_track_text(track_id);
                                    Some(
                                        list_row(
                                            ("playlist-entry", id as u64),
                                            this.selected_entry == Some(id),
                                        )
                                        .child(
                                            caption((index + 1).to_string())
                                                .w(px(20.0))
                                                .flex_shrink_0(),
                                        )
                                        .child(row_text(title, detail))
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, window, cx| {
                                                this.focus_workspace(window, cx);
                                                this.selected_entry = Some(id);
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
                    .flex_1()
                    .min_h_0()
                    .w_full(),
                );
            if self.playlist_delete_confirm == Some(playlist_id) {
                panel = panel.child(
                    column()
                        .p(gpui::px(UI_INSET))
                        .bg(rgb(ERROR_BG))
                        .border_1()
                        .border_color(rgb(ERROR))
                        .rounded_sm()
                        .child(caption(
                            "This playlist is not empty. Delete it and all entries?",
                        ))
                        .child(
                            row()
                                .child(button(
                                    ("playlist-delete-confirm", playlist_id as u64),
                                    "Delete playlist",
                                    cx,
                                    move |this, _, cx| {
                                        this.playlist_delete_confirm = None;
                                        this.selected_playlist = None;
                                        this.selected_entry = None;
                                        this.send(Command::DeletePlaylist { playlist_id }, cx);
                                    },
                                ))
                                .child(button(
                                    ("playlist-delete-cancel", playlist_id as u64),
                                    "Cancel",
                                    cx,
                                    |this, _, cx| {
                                        this.playlist_delete_confirm = None;
                                        cx.notify();
                                    },
                                )),
                        ),
                );
            }
        } else {
            panel = panel.child(caption("Create, import or select a playlist."));
        }
        panel.into_any_element()
    }

    pub(super) fn metadata_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut panel = column()
            .id(("metadata-panel", panel_id))
            .size_full()
            .overflow_y_scroll();
        if let Some(track) = self
            .metadata_track
            .and_then(|id| self.library_index.get(&id))
            .and_then(|index| self.state.library.get(*index))
        {
            let id = track.id;
            let favorite = track.favorite;
            panel = panel
                .child(caption(format!(
                    "Track #{id} · {}",
                    if track.missing {
                        "File missing"
                    } else {
                        "File available"
                    }
                )))
                .child(caption(format!(
                    "Codec: {} · Sample rate: {} · Channels: {}",
                    if track.codec.is_empty() {
                        "—"
                    } else {
                        &track.codec
                    },
                    if track.sample_rate == 0 {
                        "—".into()
                    } else {
                        format!("{} Hz", track.sample_rate)
                    },
                    if track.channels == 0 {
                        "—".into()
                    } else {
                        track.channels.to_string()
                    }
                )))
                .child(caption(format!(
                    "Duration: {}",
                    track.duration.map_or("—".into(), format_time)
                )))
                .child(caption(format!(
                    "Bitrate: {} · Source bits/sample: {}",
                    track.bitrate_bps.map_or("—".into(), |value| format!(
                        "{:.1} kbps",
                        value as f64 / 1000.0
                    )),
                    track
                        .bits_per_sample
                        .map_or("—".into(), |value| value.to_string())
                )))
                .child(caption(format!(
                    "Disc: {} · Track: {} · Release date: {}",
                    track
                        .disc_number
                        .map_or("—".into(), |value| value.to_string()),
                    track
                        .track_number
                        .map_or("—".into(), |value| value.to_string()),
                    track.release_date.as_deref().unwrap_or("—")
                )))
                .child(caption(format!(
                    "{} plays · {}",
                    track.play_count,
                    track
                        .last_played
                        .map_or("Last played —".into(), last_played_text)
                )))
                .when_some(track.cue.as_ref(), |panel, cue| {
                    panel.child(caption(format!(
                        "CUE: {} · Track {} · {}–{}",
                        cue.sheet.display(),
                        cue.number,
                        format_time(cue.start_seconds()),
                        cue.end_seconds().map_or("—".into(), format_time)
                    )))
                })
                .child(button(
                    ("metadata-favorite", panel_id),
                    if favorite {
                        "Favorite: on (clear)"
                    } else {
                        "Favorite: off (set)"
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
                ))
                .child(caption(track.path.to_string_lossy().into_owned()).truncate())
                .child(self.panel_field(Field::Title, "Title"))
                .child(self.panel_field(Field::Artist, "Artist"))
                .child(self.panel_field(Field::Album, "Album"))
                .child(row().flex_wrap().child(button(
                    ("metadata-save", panel_id),
                    "Save metadata",
                    cx,
                    move |this, _, cx| {
                        this.send(
                            Command::EditTrack {
                                track_id: id,
                                title: this.value(Field::Title, cx),
                                artist: this.value(Field::Artist, cx),
                                album: this.value(Field::Album, cx),
                            },
                            cx,
                        );
                    },
                )))
                .child(caption(
                    "Edits are stored in your library. Media files are never modified.",
                ));
        } else {
            panel = panel.child(caption(
                "Select a Library track to edit title, artist and album.",
            ));
        }
        if let Some(id) = self.state.current_track().map(|track| track.id) {
            panel = panel.child(row().child(button(
                ("metadata-current", panel_id),
                "Edit playing track",
                cx,
                move |this, _, cx| {
                    this.select_track(id, false, cx);
                },
            )));
        }
        panel.into_any_element()
    }

    pub(super) fn history_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        column()
            .id(("history-panel", panel_id))
            .size_full()
            .child(caption(format!(
                "Recent tracks · {} tracks · newest first",
                self.state.history.len()
            )))
            .when(self.state.history.is_empty(), |panel| {
                panel.child(caption("Started tracks will appear here once per track."))
            })
            .child(
                uniform_list(
                    ("history-rows", panel_id),
                    self.state.history.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let item = this.state.history.get(index)?;
                                let track_id = item.track_id;
                                let mut item_row = list_row(
                                    ("history-entry", track_id as u64),
                                    this.selected.contains(&track_id),
                                )
                                .child(row_text(
                                    item.title.clone(),
                                    last_played_text(item.played_at),
                                ));
                                if this.library_index.contains_key(&track_id) {
                                    item_row = item_row.on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, _, cx| {
                                            this.select_track(track_id, false, cx);
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id }, cx);
                                            }
                                        },
                                    ));
                                }
                                Some(item_row)
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0()
                .w_full(),
            )
            .child(div().h(px(1.0)).flex_shrink_0().bg(rgb(BORDER)))
            .child(caption("Most played · ranked by play count"))
            .child(
                uniform_list(
                    ("most-played-rows", panel_id),
                    self.most_played.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let track =
                                    this.state.library.get(*this.most_played.get(index)?)?;
                                let id = track.id;
                                Some(
                                    list_row(
                                        ("most-played-track", id as u64),
                                        this.selected.contains(&id),
                                    )
                                    .child(
                                        caption((index + 1).to_string())
                                            .w(px(24.0))
                                            .flex_shrink_0(),
                                    )
                                    .child(row_text(
                                        track.title.clone(),
                                        format!("{} plays", track.play_count),
                                    ))
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
                .flex_1()
                .min_h_0()
                .w_full(),
            )
            .into_any_element()
    }
}
