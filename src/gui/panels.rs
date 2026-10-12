use super::{
    ACCENT, BORDER, Dragging, Field, GuiApp, HIGHLIGHT, HistorySort, ListFocus, MUTED, Measured,
    StatusKind, TEXT, UI_INSET, ViewRequestKind, active_control, artwork_preview, column,
    form_field, format_time, icon_button, library::LibraryDrag, list_viewport, metadata_item,
    panel_header, panel_surface, panel_toolbar, row, row_text, section_group, sortable_header,
    status_message, track_row, two_column,
};
pub(super) use super::{TRACK_HEIGHT, caption, list_row};
use crate::{
    gui::QueueDrag,
    model::{Command, PlaybackStatus, RepeatMode},
};
use chrono::{DateTime, Local};
use gpui::{
    AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb, uniform_list,
};

fn metadata_pair(label: &'static str, value: impl Into<SharedString>) -> Div {
    metadata_item(label, value)
}

fn metadata_row(first: Div, second: Div) -> Div {
    two_column(first, second)
}

fn metadata_group(title: &'static str, content: impl IntoElement) -> Div {
    section_group(title, content)
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
        let preview_position = self
            .interaction
            .seek_preview()
            .unwrap_or(self.state.playback.position);
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
            .child(active_control(
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
                .text_2xl(),
                self.state.playback.shuffle,
            ))
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
            .child(active_control(
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
                .text_2xl(),
                self.state.playback.repeat != RepeatMode::Off,
            ));
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
                    .font_family(super::fonts::SYMBOLS_FAMILY)
                    .child("󰕾")
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
        let scroll = self.scroll_views.content(("transport", panel_id));
        div()
            .id(("transport-panel", panel_id))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&scroll)
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
                                            .font_family(super::fonts::SYMBOLS_FAMILY)
                                            .child("󰅐")
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
        form_field(label, self.input(field))
    }

    fn panel_selected_tracks(&self) -> Vec<i64> {
        if self.library_range_pending() {
            return Vec::new();
        }
        let mut tracks: Vec<_> = self.browser.library.selected().iter().copied().collect();
        // Match library drag ordering instead of exposing HashSet iteration order.
        tracks.sort_unstable();
        tracks
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
            .browser
            .library
            .buffer()
            .rows
            .iter()
            .find(|row| row.id == track_id)
            .or_else(|| {
                self.browser
                    .history
                    .rows()
                    .iter()
                    .find(|row| row.id == track_id)
            })
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
            .browser
            .playlists
            .entries()
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

    fn library_scan_status(&self, panel_id: u64, cx: &mut Context<Self>) -> Div {
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
        let mut header = row()
            .items_center()
            .gap_2()
            .child(caption(format!("{phase} · {count} · {} errors", progress.errors)).flex_1());
        if !self.state.system.scanning
            && matches!(progress.phase, ScanPhase::Completed | ScanPhase::Failed)
        {
            header = header.child(super::components::quiet_icon_button(icon_button(
                ("dismiss-scan-result", panel_id),
                "",
                "Dismiss scan result",
                cx,
                |this, _, cx| this.send(Command::DismissScanResult, cx),
            )));
        }
        let mut status = column().gap_1().flex_shrink_0().child(header);
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

    pub(super) fn library_favorite_toggle(
        &self,
        id: impl Into<gpui::ElementId>,
        track_id: i64,
        favorite: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        super::components::quiet_icon_button(icon_button(
            id,
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
                        track_ids: vec![track_id],
                        favorite: !favorite,
                    },
                    cx,
                );
            },
        ))
        .text_color(rgb(if favorite { ACCENT } else { MUTED }))
        .opacity(0.)
        .group_hover("library-track-row", |style| style.opacity(1.))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
    }

    pub(super) fn library_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let total = if self.browser.library.tree_active() {
            self.browser.library.tree().root_total()
        } else {
            self.browser.library.buffer().total
        };
        let tools = panel_toolbar()
            .child(icon_button(
                ("library-view", panel_id),
                if self.browser.library.tree_active() {
                    "󰉋"
                } else {
                    "󰉢"
                },
                if self.browser.library.tree_active() {
                    "Switch to flat list view"
                } else {
                    "Switch to directory tree view"
                },
                cx,
                |this, _, cx| this.toggle_library_view(cx),
            ))
            .child(active_control(
                icon_button(
                    ("library-filter-favorites", panel_id),
                    "󰓎",
                    "Toggle favorites filter",
                    cx,
                    |this, _, cx| {
                        this.refresh_favorite_filter(cx);
                    },
                ),
                self.browser.library.favorites_only(),
            ))
            .child(active_control(
                icon_button(
                    ("library-filter-missing", panel_id),
                    "󰌶",
                    "Toggle missing filter",
                    cx,
                    |this, _, cx| {
                        this.browser.library.toggle_missing();
                        this.refresh_filter(cx);
                    },
                ),
                self.browser.library.missing_only(),
            ))
            .child(icon_button(
                ("library-add-root", panel_id),
                "󰐕",
                "Add library folder",
                cx,
                |this, _, cx| this.choose_library_root(cx),
            ))
            .child(active_control(
                icon_button(
                    ("library-force-scan", panel_id),
                    "󰑐",
                    if self.browser.library.force_scan() {
                        "Force rescans: on"
                    } else {
                        "Force rescans: off"
                    },
                    cx,
                    |this, _, cx| {
                        this.browser.library.toggle_force_scan();
                        cx.notify();
                    },
                ),
                self.browser.library.force_scan(),
            ));
        let range_pending = self.library_range_pending();
        let selected_directories = self
            .browser
            .library
            .selected_nodes()
            .iter()
            .filter(|node| matches!(node, super::library::LibraryNode::Directory(_)))
            .count();
        let selection_label = if range_pending {
            "Selecting…".to_owned()
        } else if selected_directories > 0 && self.browser.library.selected().is_empty() {
            format!("{selected_directories} folders selected")
        } else if selected_directories > 0 {
            format!(
                "{} tracks · {selected_directories} folders selected",
                self.browser.library.selected().len()
            )
        } else if !self.browser.library.selected().is_empty() {
            format!("{} tracks selected", self.browser.library.selected().len())
        } else {
            String::new()
        };
        let has_selection = !selection_label.is_empty();
        let mut selection_tools = panel_toolbar()
            .h(gpui::rems(2.))
            .min_h(gpui::rems(2.))
            .child(caption(selection_label).flex_1().min_w_0().truncate());
        if !range_pending && !self.browser.library.selected().is_empty() {
            selection_tools = selection_tools
                .child(icon_button(
                    ("library-enqueue", panel_id),
                    "󰐕",
                    "Append selected tracks to queue",
                    cx,
                    |this, _, cx| {
                        if this.library_range_pending() {
                            return;
                        }
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
                        if this.library_range_pending() {
                            return;
                        }
                        this.send(
                            Command::RemoveTracks {
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        )
                    },
                ));
        }
        let mut panel = panel_surface(("library-panel", panel_id))
            .child(panel_header("Library", format!("{total} entries")))
            .child(self.panel_field(Field::Search, "Search title, artist or album"))
            .child(tools)
            .when(has_selection, |panel| panel.child(selection_tools));
        if self.state.system.scan_progress.is_some() || self.state.system.scanning {
            panel = panel.child(self.library_scan_status(panel_id, cx));
        }
        if self.browser.library.tree_active() {
            self.scroll_views.register_list(
                ("library-tree", panel_id),
                &self.browser.library.tree().scroll,
            );
            panel
                .child(self.library_directory_list(panel_id, cx))
                .into_any_element()
        } else {
            let rows = self.browser.library.buffer().rows.len();
            self.scroll_views.register_list(
                ("library-flat", panel_id),
                &self.browser.library.buffer().scroll,
            );
            panel
                .child(list_viewport(
                    uniform_list(
                        ("library-rows", panel_id),
                        rows,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let Some(row) =
                                        this.browser.library.buffer().rows.get(index).cloned()
                                    else {
                                        return this
                                            .paged_loading_row(ViewRequestKind::Library, index, cx)
                                            .into_any_element();
                                    };
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
                                    track_row(
                                        ("library-track", id as u64),
                                        ("library-track-text", id as u64),
                                        this.browser.library.selected().contains(&id),
                                        row.title,
                                        detail,
                                    )
                                    .group("library-track-row")
                                    .child(this.library_favorite_toggle(
                                        ("library-track-favorite", id as u64),
                                        id,
                                        row.favorite,
                                        cx,
                                    ))
                                    .on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, window, cx| {
                                            this.focus_layout(window, cx);
                                            this.browser.focus_list(ListFocus::Library);
                                            this.select_library_row(
                                                super::library::LibraryNode::Track(id),
                                                index,
                                                event.modifiers().shift,
                                                event.modifiers().control
                                                    || event.modifiers().platform,
                                                cx,
                                            );
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id: id }, cx);
                                            }
                                        },
                                    ))
                                    .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .with_decoration(super::paged::viewport_demand(cx.processor(
                        |this, range, _, cx| {
                            this.ensure_library_range(range, cx);
                        },
                    )))
                    .h_full()
                    .min_h_0()
                    .w_full()
                    .track_scroll(&self.browser.library.buffer().scroll),
                ))
                .into_any_element()
        }
    }

    fn selected_queue_ids_in_order(&self) -> Vec<u64> {
        self.state
            .queue
            .entries
            .iter()
            .filter(|entry| self.browser.queue.contains(&entry.id))
            .map(|entry| entry.id)
            .collect()
    }

    fn selected_queue_single(&self) -> Option<u64> {
        if self.browser.queue.len() != 1 {
            return None;
        }
        self.state
            .queue
            .entries
            .iter()
            .find(|entry| self.browser.queue.contains(&entry.id))
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
        if !self.browser.queue.contains(&source_id) || self.browser.queue.len() == 1 {
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
        if self.browser.queue.contains(&target_id) {
            return;
        }
        let target_after_removal = self.state.queue.entries[..target_index]
            .iter()
            .filter(|entry| !self.browser.queue.contains(&entry.id))
            .count();
        let first_index = self
            .state
            .queue
            .entries
            .iter()
            .position(|entry| self.browser.queue.contains(&entry.id))
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
        let selection_count = self.browser.queue.len();
        let mut tools = panel_toolbar().min_h(px(32.));
        if !self.state.queue.entries.is_empty() {
            tools = tools.child(
                icon_button(
                    ("queue-save-playlist", panel_id),
                    "\u{f0193}",
                    if self.queue_save_pending {
                        "Saving queue as playlist"
                    } else {
                        "Save queue as playlist"
                    },
                    cx,
                    |this, _, cx| this.save_queue_as_playlist(cx),
                )
                .opacity(if self.queue_save_pending { 0.4 } else { 1.0 }),
            );
            if self.queue_save_pending {
                tools = tools.child(status_message("Saving…", StatusKind::Info));
            } else if let Some(feedback) = &self.queue_save_feedback {
                let kind = if feedback.is_ok() {
                    StatusKind::Success
                } else {
                    StatusKind::Error
                };
                tools = tools.child(status_message(
                    match feedback {
                        Ok(message) | Err(message) => message.clone(),
                    },
                    kind,
                ));
            }
        }
        if selection_count > 0 {
            tools = tools
                .child(icon_button(
                    ("queue-play", panel_id),
                    "󰐊",
                    "Play selected queue entry",
                    cx,
                    |this, _, cx| {
                        let queue_id = this
                            .browser
                            .queue
                            .anchor()
                            .copied()
                            .filter(|id| this.browser.queue.contains(id))
                            .or_else(|| {
                                this.state
                                    .queue
                                    .entries
                                    .iter()
                                    .find(|entry| this.browser.queue.contains(&entry.id))
                                    .map(|entry| entry.id)
                            });
                        if let Some(queue_id) = queue_id {
                            this.send(Command::PlayQueue { queue_id }, cx);
                        }
                    },
                ))
                .child(icon_button(
                    ("queue-remove", panel_id),
                    "",
                    "Remove selected queue entries",
                    cx,
                    |this, _, cx| {
                        let queue_ids = this.selected_queue_ids_in_order();
                        if !queue_ids.is_empty() {
                            this.browser.queue.clear();

                            this.send(Command::RemoveQueueEntries { queue_ids }, cx);
                            cx.notify();
                        }
                    },
                ));
            if let Some(index) = selected_index {
                if index > 0 {
                    tools = tools.child(icon_button(
                        ("queue-up", panel_id),
                        "",
                        "Move selected entry up",
                        cx,
                        |this, _, cx| this.move_selected_queue(false, cx),
                    ));
                }
                if index + 1 < self.state.queue.entries.len() {
                    tools = tools.child(icon_button(
                        ("queue-down", panel_id),
                        "",
                        "Move selected entry down",
                        cx,
                        |this, _, cx| this.move_selected_queue(true, cx),
                    ));
                }
            }
        }
        let scroll = self.scroll_views.list(("queue", panel_id));
        panel_surface(("queue-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                this.resolve_library_drag(drag.nodes(), super::DragDestination::Queue, cx);
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
            .when(!self.state.queue.entries.is_empty(), |panel| {
                panel.child(tools)
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
                                    list_row(("queue-entry", id), this.browser.queue.contains(&id))
                                        .child(
                                            div()
                                                .w(px(32.0))
                                                .flex_shrink_0()
                                                .text_xs()
                                                .whitespace_nowrap()
                                                .text_color(rgb(if playing {
                                                    ACCENT
                                                } else {
                                                    MUTED
                                                }))
                                                .when(playing, |view| {
                                                    view.font_family(super::fonts::SYMBOLS_FAMILY)
                                                })
                                                .child(if playing {
                                                    "󰐊".to_owned()
                                                } else {
                                                    (index + 1).to_string()
                                                }),
                                        )
                                        .child(row_text(("queue-entry-text", id), title, detail))
                                        .on_mouse_down(
                                            gpui::MouseButton::Right,
                                            cx.listener(move |this, _, window, cx| {
                                                this.focus_layout(window, cx);
                                                this.browser.focus_list(ListFocus::Queue);
                                                if !this.browser.queue.contains(&id) {
                                                    this.browser.queue.select(id, false);
                                                    cx.notify();
                                                }
                                            }),
                                        )
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, window, cx| {
                                                this.focus_layout(window, cx);
                                                this.browser.focus_list(ListFocus::Queue);
                                                let modifiers = event.modifiers();
                                                this.select_queue_entry(
                                                    id,
                                                    modifiers.shift,
                                                    modifiers.control || modifiers.platform,
                                                    cx,
                                                );
                                                if event.click_count() == 2 {
                                                    this.send(
                                                        Command::PlayQueue { queue_id: id },
                                                        cx,
                                                    );
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
                    this.resolve_library_drag(drag.nodes(), super::DragDestination::Queue, cx);
                }))
                .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
                .h_full()
                .min_h_0()
                .w_full()
                .track_scroll(&scroll),
            ))
            .into_any_element()
    }

    fn move_selected_playlist_entry(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(entry_id) = self.browser.playlists.selected_entry() else {
            return;
        };
        let Some(index) = self
            .browser
            .playlists
            .entries()
            .rows
            .position(|entry| entry.id == entry_id)
        else {
            return;
        };
        let target = if down {
            index.saturating_add(1)
        } else {
            index.saturating_sub(1)
        };
        if target != index && target < self.browser.playlists.entries().total {
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
        let playlist_id = self.browser.playlists.selected();
        let selected_name = playlist_id
            .and_then(|id| {
                self.browser
                    .playlists
                    .summaries()
                    .rows
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_else(|| self.browser.playlists.selected_name().to_owned());
        let entry_count = if playlist_id == self.browser.playlists.entries().playlist_id {
            self.browser.playlists.entries().total
        } else {
            0
        };
        self.scroll_views.register_list(
            ("playlist-catalog", panel_id),
            &self.browser.playlists.summaries().scroll,
        );
        let mut panel = panel_surface(("playlists-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                if let Some(id) = this.browser.playlists.selected() {
                    this.resolve_library_drag(
                        drag.nodes(),
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
                    self.browser.playlists.summaries().total
                ),
            ))
            .child(
                row()
                    .items_end()
                    .gap_2()
                    .child(
                        self.panel_field(Field::PlaylistName, "Playlist name")
                            .flex_1(),
                    )
                    .child(icon_button(
                        ("playlist-create", panel_id),
                        "",
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
                    )),
            )
            .child(panel_toolbar().child(icon_button(
                ("playlist-import", panel_id),
                "",
                "Import M3U",
                cx,
                |this, _, cx| this.choose_playlist_import(cx),
            )))
            .child(list_viewport(
                uniform_list(
                    ("playlist-catalog", panel_id),
                    self.browser.playlists.summaries().rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let Some(p) =
                                    this.browser.playlists.summaries().rows.get(index).cloned()
                                else {
                                    return this
                                        .paged_loading_row(
                                            ViewRequestKind::PlaylistSummaries,
                                            index,
                                            cx,
                                        )
                                        .into_any_element();
                                };
                                let id = p.id;
                                let name = p.name.clone();
                                list_row(
                                    ("playlist", id as u64),
                                    this.browser.playlists.selected() == Some(id),
                                )
                                .child(row_text(
                                    ("playlist-text", id as u64),
                                    p.name,
                                    format!("{} tracks", p.entry_count),
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.browser.playlists.select(id, name.clone(), index) {
                                        this.request_playlist_entries(id, 0, cx);
                                    }
                                    cx.notify();
                                }))
                                .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .with_decoration(super::paged::viewport_demand(cx.processor(
                    |this, range, _, cx| {
                        this.ensure_playlist_range(range, cx);
                    },
                )))
                .flex_1()
                .min_h_0()
                .w_full()
                .track_scroll(&self.browser.playlists.summaries().scroll),
            ));
        if let Some(id) = playlist_id {
            self.scroll_views.register_list(
                ("playlist-entries", panel_id),
                &self.browser.playlists.entries().scroll,
            );
            panel = panel
                .child(caption(format!("{} · {entry_count} tracks", selected_name)).truncate())
                .child(
                    panel_toolbar()
                        .child(icon_button(
                            ("playlist-add", panel_id),
                            "",
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
                            "󰐊",
                            "Play playlist",
                            cx,
                            move |this, _, cx| {
                                this.send(Command::PlayPlaylist { playlist_id: id }, cx)
                            },
                        ))
                        .child(icon_button(
                            ("playlist-export", panel_id),
                            "",
                            "Export playlist",
                            cx,
                            move |this, _, cx| this.choose_playlist_export(id, cx),
                        ))
                        .child(icon_button(
                            ("playlist-rename", panel_id),
                            "",
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
                            "",
                            "Delete playlist",
                            cx,
                            move |this, _, cx| {
                                this.browser.remove_playlist_selection();
                                this.send(Command::DeletePlaylist { playlist_id: id }, cx);
                            },
                        ))
                        .child(caption("Playlist").text_xs().text_color(rgb(MUTED)))
                        .child(div().w(px(1.)).h(px(20.)).bg(rgb(BORDER)))
                        .when(self.browser.playlists.selected_entry().is_some(), |bar| {
                            bar.child(icon_button(
                                ("entry-play", panel_id),
                                "󰐊",
                                "Play selected track",
                                cx,
                                move |this, _, cx| {
                                    let track_id = this
                                        .browser
                                        .playlists
                                        .entries()
                                        .rows
                                        .iter()
                                        .find(|entry| {
                                            Some(entry.id)
                                                == this.browser.playlists.selected_entry()
                                        })
                                        .map(|entry| entry.track_id);
                                    if let Some(track_id) = track_id {
                                        this.send(Command::Play { track_id }, cx);
                                    }
                                },
                            ))
                            .child(icon_button(
                                ("entry-remove", panel_id),
                                "",
                                "Remove selected entry",
                                cx,
                                move |this, _, cx| {
                                    if let Some(entry_id) = this.browser.playlists.take_entry() {
                                        this.send(Command::RemovePlaylistEntry { entry_id }, cx);
                                    }
                                },
                            ))
                            .child(icon_button(
                                ("entry-up", panel_id),
                                "",
                                "Move entry up",
                                cx,
                                move |this, _, cx| this.move_selected_playlist_entry(false, cx),
                            ))
                            .child(icon_button(
                                ("entry-down", panel_id),
                                "",
                                "Move entry down",
                                cx,
                                move |this, _, cx| this.move_selected_playlist_entry(true, cx),
                            ))
                        }),
                )
                .child(list_viewport(
                    uniform_list(
                        ("playlist-entries", panel_id),
                        self.browser.playlists.entries().rows.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let Some(e) =
                                        this.browser.playlists.entries().rows.get(index).cloned()
                                    else {
                                        return this
                                            .paged_loading_row(
                                                ViewRequestKind::PlaylistEntries,
                                                index,
                                                cx,
                                            )
                                            .into_any_element();
                                    };
                                    let id = e.id;
                                    let track_id = e.track_id;
                                    list_row(
                                        ("playlist-entry", id as u64),
                                        this.browser.playlists.selected_entry() == Some(id),
                                    )
                                    .child(caption((index + 1).to_string()).w(px(28.)))
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
                                            this.focus_layout(window, cx);
                                            this.browser.playlists.select_entry(id, index);
                                            this.request_track(track_id, cx);
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id }, cx);
                                            }
                                            cx.notify();
                                        },
                                    ))
                                    .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .with_decoration(super::paged::viewport_demand(cx.processor(
                        |this, range, _, cx| {
                            this.ensure_entry_range(range, cx);
                        },
                    )))
                    .h_full()
                    .min_h_0()
                    .w_full()
                    .track_scroll(&self.browser.playlists.entries().scroll),
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
        self.prepare_artwork(cx);
        let mut panel = panel_surface(("metadata-panel", panel_id));
        if let Some(track) = self.metadata.track() {
            let id = track.id;
            let favorite = track.favorite;
            let metadata_dirty = self.metadata_dirty(cx) && !self.metadata.saving();
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
            let scroll = self.scroll_views.content(("metadata", panel_id));
            let details = div()
                .id(("metadata-details", panel_id))
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .child(
                    column()
                        .flex_shrink_0()
                        .p(px(UI_INSET))
                        .gap_3()
                        .child(
                            row()
                                .justify_between()
                                .items_center()
                                .child(
                                    caption(format!("Track #{id}"))
                                        .text_lg()
                                        .text_color(rgb(TEXT)),
                                )
                                .child(
                                    row()
                                        .gap_2()
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
                                        ))
                                        .child(
                                            icon_button(
                                                ("metadata-save", panel_id),
                                                "󰆓",
                                                "Save metadata. Media files are never modified.",
                                                cx,
                                                |this, _, cx| this.save_metadata(cx),
                                            )
                                            .when(!metadata_dirty, |view| {
                                                view.opacity(0.4).cursor_default()
                                            }),
                                        ),
                                ),
                        )
                        .child(metadata_group(
                            "File",
                            metadata_row(
                                metadata_pair("File path", file_path),
                                metadata_pair(
                                    "Availability",
                                    if track.missing {
                                        "Missing"
                                    } else {
                                        "Available"
                                    },
                                ),
                            ),
                        ))
                        .child(metadata_group(
                            "Metadata",
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
                                    metadata_pair(
                                        "Duration",
                                        track.duration.map_or("—".into(), format_time),
                                    ),
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
                                )),
                        ))
                        .child(metadata_group(
                            "Tags",
                            column()
                                .gap_2()
                                .child(metadata_row(
                                    self.panel_field(Field::Title, "Title"),
                                    self.panel_field(Field::Artist, "Artist"),
                                ))
                                .child(metadata_row(
                                    self.panel_field(Field::Album, "Album"),
                                    metadata_pair(
                                        "Disc",
                                        track
                                            .disc_number
                                            .map_or("—".into(), |value| value.to_string()),
                                    ),
                                ))
                                .child(metadata_row(
                                    metadata_pair(
                                        "Track",
                                        track
                                            .track_number
                                            .map_or("—".into(), |value| value.to_string()),
                                    ),
                                    metadata_pair(
                                        "Release",
                                        track.release_date.as_deref().unwrap_or("—"),
                                    ),
                                ))
                                .child(metadata_row(
                                    metadata_pair("Play count", track.play_count.to_string()),
                                    metadata_pair("Last played", last_played),
                                ))
                                .when_some(
                                    self.artwork_manager.cached_path(track),
                                    |tags, path| tags.child(artwork_preview(path)),
                                ),
                        )),
                );
            panel = panel.child(details);
        }
        panel.into_any_element()
    }

    fn history_sort_button(
        &self,
        panel_id: u64,
        label: &'static str,
        sort: HistorySort,
        width: Option<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.browser.history.sort() == sort;
        let id = match sort {
            HistorySort::Recent => ("history-sort-recent", panel_id),
            HistorySort::Title => ("history-sort-title", panel_id),
            HistorySort::Plays => ("history-sort-plays", panel_id),
        };
        sortable_header(
            id,
            label,
            active,
            self.browser.history.descending(),
            width,
            cx,
            move |this, _, cx| {
                this.browser.history.set_sort(sort);
                cx.notify();
            },
        )
    }

    pub(super) fn history_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut history = self.state.library.history.as_ref().clone();
        let sort = self.browser.history.sort();
        let descending = self.browser.history.descending();
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
            .min_w_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(caption("#").w(px(32.)).flex_shrink_0())
            .child(self.history_sort_button(panel_id, "Track", HistorySort::Title, None, cx))
            .child(self.history_sort_button(
                panel_id,
                "Play count",
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
        let scroll = self.scroll_views.list(("history", panel_id));
        panel_surface(("history-panel", panel_id))
            .child(panel_header(
                "Recent tracks",
                format!("{} tracks", history_len),
            ))
            .child(list_viewport(
                column().size_full().child(header).child(
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
                                    let last_played =
                                        this.last_played_text(id, Some(item.played_at));
                                    let last_played = last_played
                                        .strip_prefix("Last played · ")
                                        .unwrap_or(&last_played);
                                    Some(
                                        list_row(
                                            ("history-entry", id as u64),
                                            this.browser.library.selected().contains(&id),
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
                                            caption(last_played.to_owned())
                                                .w(px(148.))
                                                .flex_shrink_0()
                                                .truncate(),
                                        )
                                        .on_click(
                                            cx.listener(
                                                move |this, event: &gpui::ClickEvent, _, cx| {
                                                    this.select_track(id, false, cx);
                                                    if event.click_count() == 2 {
                                                        this.send(
                                                            Command::Play { track_id: id },
                                                            cx,
                                                        );
                                                    }
                                                },
                                            ),
                                        ),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .h_full()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .track_scroll(&scroll),
                ),
            ))
            .into_any_element()
    }
}
