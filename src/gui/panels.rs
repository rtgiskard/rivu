use super::{
    ACCENT, BORDER, Dragging, Field, GuiApp, HIGHLIGHT, HistorySort, ListFocus, MUTED, Measured,
    StatusKind, TEXT, UI_INSET, ViewRequestKind, active_control, artwork_preview, button, column,
    components::{danger_control, disabled_control, quiet_icon_button},
    form_field, format_time, icon_button,
    library::LibraryDrag,
    list_viewport, metadata_item, paged, panel_header, panel_surface, panel_toolbar, row, row_text,
    section_group, sortable_header, status_message,
};
pub(super) use super::{TRACK_HEIGHT, caption, list_row};
use crate::{
    gui::QueueDrag,
    model::{Command, DecoderHint, PlaybackStatus, RepeatMode},
};
use chrono::{DateTime, Local};
use gpui::{
    AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb, uniform_list,
};

fn metadata_rows(
    rows: impl IntoIterator<Item = [(&'static str, SharedString); 2]>,
    label_widths: [gpui::Pixels; 2],
) -> Div {
    column()
        .gap_1()
        .children(rows.into_iter().map(|[first, second]| {
            metadata_row(
                metadata_item(first.0, first.1, label_widths[0]),
                metadata_item(second.0, second.1, label_widths[1]),
            )
        }))
}

fn metadata_row(first: Div, second: Div) -> Div {
    div()
        .grid()
        .grid_cols(2)
        .gap_4()
        .items_start()
        .child(first.min_w_0())
        .child(second.min_w_0())
}

fn metadata_group(title: &'static str, content: impl IntoElement) -> Div {
    section_group(title, content)
}

#[derive(Clone, Copy)]
pub(super) struct PlaylistDrag {
    playlist_id: i64,
    entry_id: i64,
    position: usize,
    revision: u64,
    catalog_revision: u64,
    selection_generation: u64,
}

impl gpui::Render for PlaylistDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        super::drag_preview("Move playlist entry")
    }
}

#[derive(Clone)]
pub(super) struct PlaylistTarget {
    id: i64,
    name: String,
}

#[derive(Clone)]
pub(super) enum PlaylistAction {
    Menu(PlaylistTarget, gpui::Point<gpui::Pixels>),
    Play(PlaylistTarget),
    Delete(PlaylistTarget),
}

fn track_list_row(
    id: impl Into<gpui::ElementId>,
    text_id: impl Into<gpui::ElementId>,
    title: String,
    detail: String,
    index: usize,
    playing: bool,
    selected: bool,
) -> gpui::Stateful<Div> {
    list_row(id, selected)
        .child(
            div()
                .w(px(32.))
                .flex_shrink_0()
                .text_xs()
                .whitespace_nowrap()
                .text_color(rgb(if playing { ACCENT } else { MUTED }))
                .when(playing, |view| {
                    view.font_family(super::fonts::SYMBOLS_FAMILY)
                })
                .child(if playing {
                    "󰐊".to_owned()
                } else {
                    (index + 1).to_string()
                }),
        )
        .child(row_text(text_id, title, detail))
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
                quiet_icon_button(icon_button(
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
                ))
                .rounded_full()
                .text_lg(),
                self.state.playback.shuffle,
            ))
            .child(
                quiet_icon_button(icon_button(
                    ("previous", panel_id),
                    "󰒮",
                    "Previous track (p)",
                    cx,
                    |this, _, cx| this.send(Command::Previous, cx),
                ))
                .rounded_full()
                .text_lg(),
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
                .size(gpui::rems(2.5))
                .rounded_full()
                .border_0()
                .text_xl()
                .bg(rgb(HIGHLIGHT))
                .text_color(rgb(ACCENT)),
            )
            .child(
                quiet_icon_button(icon_button(
                    ("next", panel_id),
                    "󰒭",
                    "Next track (n)",
                    cx,
                    |this, _, cx| this.send(Command::Next, cx),
                ))
                .rounded_full()
                .text_lg(),
            )
            .child(active_control(
                quiet_icon_button(icon_button(
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
                ))
                .rounded_full()
                .text_lg(),
                self.state.playback.repeat != RepeatMode::Off,
            ));
        let volume_control = row()
            .w(gpui::rems(8.6))
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
                    .w(gpui::rems(2.5))
                    .whitespace_nowrap()
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
                            .gap_0()
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
    pub(super) fn panel_track_text(track: &crate::model::LibraryRow) -> (String, String) {
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
        (track.title.clone(), detail)
    }

    pub(super) fn panel_error(&mut self, message: &'static str, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    fn library_scan_status(&self, panel_id: u64, cx: &mut Context<Self>) -> Div {
        use crate::model::ScanPhase;
        let progress = self.state.system.scan_progress.as_ref();
        let header_text = progress.map_or_else(
            || "Scanning library…".to_owned(),
            |progress| {
                let phase = match progress.phase {
                    ScanPhase::Discovering => "Discovering files",
                    ScanPhase::ReadingMetadata => "Reading metadata",
                    ScanPhase::Saving => "Saving library",
                    ScanPhase::Completed => "Scan completed",
                    ScanPhase::Failed => "Scan failed",
                    ScanPhase::Cancelled => "Scan cancelled",
                };
                let count = match progress.total {
                    Some(total) => format!("{} / {total} files", progress.processed),
                    None => format!("{} files found", progress.processed),
                };
                format!("{phase} · {count} · {} errors", progress.errors)
            },
        );
        let dismissible = !self.state.system.scanning
            && progress.is_some_and(|progress| {
                matches!(
                    progress.phase,
                    ScanPhase::Completed | ScanPhase::Failed | ScanPhase::Cancelled
                )
            });
        let dismiss = div()
            .size(gpui::rems(2.))
            .flex_shrink_0()
            .when(dismissible, |slot| {
                slot.child(quiet_icon_button(icon_button(
                    ("dismiss-scan-result", panel_id),
                    "",
                    "Dismiss scan result",
                    cx,
                    |this, _, cx| this.send(Command::DismissScanResult, cx),
                )))
            });
        let mut bar = div().w_full().h(px(3.)).flex_shrink_0().bg(rgb(BORDER));
        if let Some(progress) = progress
            && let Some(total) = progress.total.filter(|total| *total > 0)
        {
            let fraction = (progress.processed as f32 / total as f32).clamp(0., 1.);
            bar = bar.child(div().h_full().w(gpui::relative(fraction)).bg(rgb(
                if progress.phase == ScanPhase::Failed {
                    super::ERROR
                } else {
                    ACCENT
                },
            )));
        }
        let path: SharedString = progress
            .and_then(|progress| progress.path.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let message: SharedString = self.state.system.scan_message.clone().into();
        // Keep every scan phase in the same cells; only dismissal removes the surface.
        column()
            .gap_1()
            .min_w_0()
            .flex_shrink_0()
            .child(
                row()
                    .h(gpui::rems(2.))
                    .flex_shrink_0()
                    .child(caption(header_text).flex_1().min_w_0().truncate())
                    .child(dismiss),
            )
            .child(bar)
            .child(
                caption(path.clone())
                    .id(("scan-path", panel_id))
                    .h(gpui::rems(1.25))
                    .flex_shrink_0()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::ButtonTooltip { text: path.clone() })
                            .into()
                    }),
            )
            .child(
                caption(message.clone())
                    .id(("scan-message", panel_id))
                    .h(gpui::rems(1.25))
                    .flex_shrink_0()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::ButtonTooltip {
                            text: message.clone(),
                        })
                        .into()
                    }),
            )
    }

    pub(super) fn track_favorite_toggle(
        &self,
        id: impl Into<gpui::ElementId>,
        group: &'static str,
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
        .group_hover(group, |style| style.opacity(1.))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
    }

    pub(super) fn library_panel(
        &mut self,
        panel_id: u64,
        tab_bar_visible: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let total = self.browser.library.tree().root_total();
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
            format!("{total} entries")
        };
        let can_edit_tracks = !range_pending && !self.browser.library.selected().is_empty();
        let mut tools = panel_toolbar();
        tools = tools
            .when(!tab_bar_visible, |tools| {
                tools.child(div().text_sm().text_color(rgb(TEXT)).child("Library"))
            })
            .child(
                caption(selection_label.clone())
                    .id(("library-selection-count", panel_id))
                    .w(gpui::rems(6.))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::ButtonTooltip {
                            text: selection_label.clone().into(),
                        })
                        .into()
                    }),
            )
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
        tools = tools
            .child(disabled_control(
                icon_button(
                    ("library-enqueue", panel_id),
                    "󰐕",
                    "Append selected tracks to queue",
                    cx,
                    |this, _, cx| {
                        if this.library_range_pending()
                            || this.browser.library.selected().is_empty()
                        {
                            return;
                        }
                        this.send(
                            Command::Enqueue {
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        )
                    },
                ),
                !can_edit_tracks,
            ))
            .child(disabled_control(
                danger_control(icon_button(
                    ("library-remove", panel_id),
                    "󰆴",
                    "Remove selected tracks from library",
                    cx,
                    |this, _, cx| {
                        if this.library_range_pending()
                            || this.browser.library.selected().is_empty()
                        {
                            return;
                        }
                        this.send(
                            Command::RemoveTracks {
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        )
                    },
                )),
                !can_edit_tracks,
            ));
        let mut panel = panel_surface(("library-panel", panel_id))
            .child(tools)
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .child(self.input(Field::Search)),
            );
        if self.state.system.scan_progress.is_some() || self.state.system.scanning {
            panel = panel.child(self.library_scan_status(panel_id, cx));
        }
        self.scroll_views.register_list(
            ("library-projection", panel_id),
            &self.browser.library.tree().scroll,
        );
        panel
            .child(self.library_directory_list(panel_id, cx))
            .into_any_element()
    }
    fn send_queue_row_action(&mut self, revision: u64, command: Command, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.state.queue.revision == revision {
            self.send(command, cx);
        }
    }

    pub(super) fn selected_queue_ids_in_order(&self) -> Vec<u64> {
        self.browser.queue.ordered_ids()
    }

    fn move_queue_entry(&mut self, drag: &QueueDrag, target_id: u64, cx: &mut Context<Self>) {
        if !self.browser.queue.ready() || drag.revision != self.state.queue.revision {
            return;
        }
        let source_id = drag.queue_id;
        let source_position = drag.position;
        let Some(target) = self
            .browser
            .queue
            .rows
            .iter()
            .find(|row| row.id == target_id)
            .map(super::queue::QueueIdentity::from)
        else {
            return;
        };
        if !self.browser.queue.contains(&source_id) || self.browser.queue.len() == 1 {
            if source_position != target.position {
                self.send(
                    Command::MoveQueue {
                        queue_id: source_id,
                        index: target.position,
                    },
                    cx,
                );
            }
            return;
        }
        if self.browser.queue.contains(&target_id) {
            return;
        }
        let selected = self.browser.queue.ordered();
        let before = selected
            .iter()
            .filter(|row| row.position < target.position)
            .count();
        let first = selected
            .first()
            .map(|row| row.position)
            .unwrap_or(source_position);
        let index = target.position - before + usize::from(target.position > first);
        self.send(
            Command::MoveQueueEntries {
                queue_ids: selected.iter().map(|row| row.id).collect(),
                index,
            },
            cx,
        );
    }

    pub(super) fn queue_panel(
        &mut self,
        panel_id: u64,
        tab_bar_visible: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selection_count = self.browser.queue.len();
        let header = row()
            .flex_wrap()
            .flex_shrink_0()
            .when(!tab_bar_visible, |header| {
                header.child(div().text_sm().text_color(rgb(TEXT)).child("Queue"))
            })
            .child(
                caption(format!(
                    "{} queued · {} selected",
                    self.state.queue.total, selection_count,
                ))
                .flex_shrink_0()
                .id(("queue-count", panel_id))
                .tooltip(|_, cx| {
                    cx.new(|_| super::ButtonTooltip {
                        text: "Drag an entry onto its new position".into(),
                    })
                    .into()
                }),
            );
        let mut tools = panel_toolbar().min_h(px(32.));
        if self.state.queue.total > 0 {
            tools = tools.child(disabled_control(
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
                ),
                self.queue_save_pending,
            ));
        }
        if selection_count > 1 {
            tools = tools.child(icon_button(
                ("queue-remove", panel_id),
                "",
                "Remove selected queue entries",
                cx,
                |this, _, cx| {
                    let queue_ids = this.selected_queue_ids_in_order();
                    if queue_ids.len() > 1 {
                        this.browser.queue.clear();

                        this.send(Command::RemoveQueueEntries { queue_ids }, cx);
                        cx.notify();
                    }
                },
            ));
        }
        let (save_message, save_kind): (SharedString, _) = if self.queue_save_pending {
            ("Saving…".into(), StatusKind::Info)
        } else {
            match &self.queue_save_feedback {
                Some(Ok(message)) => (message.clone().into(), StatusKind::Success),
                Some(Err(message)) => (message.clone().into(), StatusKind::Error),
                None => ("".into(), StatusKind::Info),
            }
        };
        let (selection_message, selection_kind): (SharedString, _) =
            if let Some(error) = &self.browser.queue.selection_error {
                (error.clone().into(), StatusKind::Error)
            } else if self.browser.queue.busy {
                ("Loading selection…".into(), StatusKind::Info)
            } else {
                ("".into(), StatusKind::Info)
            };
        let status = row()
            .h(gpui::rems(2.))
            .flex_shrink_0()
            .overflow_hidden()
            .child(
                status_message(save_message.clone(), save_kind)
                    .id(("queue-save-status", panel_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::ButtonTooltip {
                            text: save_message.clone(),
                        })
                        .into()
                    }),
            )
            .child(
                status_message(selection_message.clone(), selection_kind)
                    .id(("queue-selection-status", panel_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::ButtonTooltip {
                            text: selection_message.clone(),
                        })
                        .into()
                    }),
            )
            .child(div().w(gpui::rems(8.)).h_full().flex_shrink_0().when(
                self.browser.queue.selection_error.is_some(),
                |slot| {
                    slot.child(icon_button(
                        ("queue-selection-retry", panel_id),
                        "󰑐",
                        "Retry selection",
                        cx,
                        |this, _, cx| {
                            if this.browser.queue.selection_error.is_some() {
                                this.refresh_queue(cx);
                                cx.notify();
                            }
                        },
                    ))
                },
            ));
        let scroll = self.scroll_views.list(("queue", panel_id));
        panel_surface(("queue-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                this.resolve_library_drag(drag.nodes(), super::DragDestination::Queue, cx);
            }))
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
            .child(header.when(self.state.queue.total > 0, |header| header.child(tools)))
            .child(status)
            .child(list_viewport(
                uniform_list(
                    ("queue-rows", panel_id),
                    self.state.queue.total,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let Some(entry) = this.browser.queue.rows.get(index) else {
                                    return this
                                        .paged_loading_row(ViewRequestKind::Queue, index, cx)
                                        .h(px(TRACK_HEIGHT));
                                };
                                let id = entry.id;
                                let track_id = entry.track_id;
                                let favorite = entry.track.favorite;
                                let can_move_down = index + 1 < this.state.queue.total;
                                let revision = this.state.queue.revision;
                                let (title, detail) = Self::panel_track_text(&entry.track);
                                let playing = this.state.queue.current_id == Some(id);
                                let drag = QueueDrag {
                                    queue_id: id,
                                    position: index,
                                    revision: this.state.queue.revision,
                                };
                                track_list_row(
                                    ("queue-entry", id),
                                    ("queue-entry-text", id),
                                    title,
                                    detail,
                                    index,
                                    playing,
                                    this.browser.queue.contains(&id),
                                )
                                .group("queue-track-row")
                                .child(
                                    row()
                                        .gap_0()
                                        .opacity(0.)
                                        .group_hover("queue-track-row", |style| style.opacity(1.))
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        })
                                        .child(this.track_favorite_toggle(
                                            ("queue-entry-favorite", id),
                                            "queue-track-row",
                                            track_id,
                                            favorite,
                                            cx,
                                        ))
                                        .child(quiet_icon_button(icon_button(
                                            ("queue-entry-play", id),
                                            "󰐊",
                                            "Play this queue entry",
                                            cx,
                                            move |this, _, cx| {
                                                this.send_queue_row_action(
                                                    revision,
                                                    Command::PlayQueue { queue_id: id },
                                                    cx,
                                                )
                                            },
                                        )))
                                        .child(quiet_icon_button(icon_button(
                                            ("queue-entry-remove", id),
                                            "",
                                            "Remove from queue",
                                            cx,
                                            move |this, _, cx| {
                                                this.send_queue_row_action(
                                                    revision,
                                                    Command::RemoveQueueEntries {
                                                        queue_ids: vec![id],
                                                    },
                                                    cx,
                                                )
                                            },
                                        )))
                                        .child(disabled_control(
                                            quiet_icon_button(icon_button(
                                                ("queue-entry-up", id),
                                                "",
                                                "Move this entry up",
                                                cx,
                                                move |this, _, cx| {
                                                    if index > 0 {
                                                        this.send_queue_row_action(
                                                            revision,
                                                            Command::MoveQueue {
                                                                queue_id: id,
                                                                index: index - 1,
                                                            },
                                                            cx,
                                                        );
                                                    }
                                                },
                                            )),
                                            index == 0,
                                        ))
                                        .child(disabled_control(
                                            quiet_icon_button(icon_button(
                                                ("queue-entry-down", id),
                                                "",
                                                "Move this entry down",
                                                cx,
                                                move |this, _, cx| {
                                                    if can_move_down {
                                                        this.send_queue_row_action(
                                                            revision,
                                                            Command::MoveQueue {
                                                                queue_id: id,
                                                                index: index + 1,
                                                            },
                                                            cx,
                                                        );
                                                    }
                                                },
                                            )),
                                            !can_move_down,
                                        )),
                                )
                                .on_mouse_down(
                                    gpui::MouseButton::Right,
                                    cx.listener(move |this, _, window, cx| {
                                        this.focus_layout(window, cx);
                                        this.browser.focus_list(ListFocus::Queue);
                                        if !this.browser.queue.contains(&id) {
                                            this.select_queue_entry(id, false, false, cx);
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
                                            this.send(Command::PlayQueue { queue_id: id }, cx);
                                        }
                                    },
                                ))
                                .on_drag(drag, |drag, _, _, cx| cx.new(|_| *drag))
                                .on_drop(cx.listener(move |this, drag: &QueueDrag, window, cx| {
                                    window.prevent_default();
                                    this.move_queue_entry(drag, id, cx);
                                }))
                                .drag_over::<QueueDrag>(|style, _, _, _| style.bg(rgb(HIGHLIGHT)))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .with_decoration(paged::viewport_demand(
                    cx.processor(|this, range, _, cx| this.ensure_queue_range(range, cx)),
                ))
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

    fn playlist_revision(&self, id: i64) -> u64 {
        self.state
            .library
            .playlist_entry_revisions
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    fn move_playlist_entry(&mut self, drag: &PlaylistDrag, target_id: i64, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.browser.playlists.selected() != Some(drag.playlist_id)
            || self.browser.playlists.entries().playlist_id != Some(drag.playlist_id)
            || self.browser.playlists.selection_generation() != drag.selection_generation
            || self.playlist_revision(drag.playlist_id) != drag.revision
            || self.state.library.playlist_revision != drag.catalog_revision
        {
            return;
        }
        let Some(index) = self
            .browser
            .playlists
            .entries()
            .rows
            .position(|row| row.id == target_id)
        else {
            return;
        };
        if index != drag.position {
            self.send(
                Command::MovePlaylistEntry {
                    entry_id: drag.entry_id,
                    index,
                },
                cx,
            );
        }
    }

    pub(super) fn dismiss_playlist_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.playlist_action = None;
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(super) fn playlist_action_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(action) = self.playlist_action.clone() else {
            return div().into_any_element();
        };
        if let PlaylistAction::Menu(target, position) = action {
            let id = target.id;
            let has_tracks =
                !self.library_range_pending() && !self.browser.library.selected().is_empty();
            let rename = target.clone();
            let delete = target.clone();
            let menu = column()
                .id("playlist-row-menu")
                .occlude()
                .w(px(260.))
                .p_1()
                .bg(rgb(super::PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .rounded_md()
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.dismiss_playlist_action(window, cx);
                    cx.stop_propagation();
                }))
                .child(
                    super::menu_item(
                        "playlist-row-add",
                        "Add selected Library tracks",
                        cx,
                        move |this, window, cx| {
                            let track_ids = this.panel_selected_tracks();
                            if track_ids.is_empty() {
                                return;
                            }
                            this.dismiss_playlist_action(window, cx);
                            this.send(
                                Command::AddPlaylist {
                                    playlist_id: id,
                                    track_ids,
                                },
                                cx,
                            );
                        },
                    )
                    .opacity(if has_tracks { 1. } else { 0.4 })
                    .when(!has_tracks, |view| view.cursor_default()),
                )
                .child(super::menu_item(
                    "playlist-row-rename",
                    "Rename…",
                    cx,
                    move |this, window, cx| {
                        this.playlist_action = None;
                        this.begin_playlist_name(
                            super::playlist_editor::PlaylistNameAction::Rename {
                                playlist_id: rename.id,
                            },
                            rename.name.clone(),
                            window,
                            cx,
                        );
                    },
                ))
                .child(super::menu_item(
                    "playlist-row-delete",
                    "Delete…",
                    cx,
                    move |this, _, cx| {
                        this.playlist_action = Some(PlaylistAction::Delete(delete.clone()));
                        cx.notify();
                    },
                ));
            return gpui::anchored()
                .position(position)
                .snap_to_window()
                .child(menu)
                .into_any_element();
        }
        let (target, deleting) = match action {
            PlaylistAction::Play(target) => (target, false),
            PlaylistAction::Delete(target) => (target, true),
            PlaylistAction::Menu(..) => unreachable!(),
        };
        let title = if deleting {
            format!("Delete “{}”?", target.name)
        } else {
            format!("Play “{}”?", target.name)
        };
        let detail = if deleting {
            "Only the playlist is deleted. Music files are not touched.".to_owned()
        } else {
            format!(
                "Replace all {} queued entries and start from the playlist's first track?",
                self.state.queue.total
            )
        };
        div()
            .id("playlist-action-modal")
            .occlude()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000).alpha(0.58))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(gpui::MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                column()
                    .w(px(420.))
                    .max_w(gpui::relative(0.9))
                    .p(px(UI_INSET))
                    .bg(rgb(super::PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .child(panel_header(title, detail))
                    .child(
                        row()
                            .justify_end()
                            .child(icon_button(
                                "playlist-action-cancel",
                                "",
                                "Cancel",
                                cx,
                                |this, window, cx| this.dismiss_playlist_action(window, cx),
                            ))
                            .child(danger_control(button(
                                "playlist-action-confirm",
                                if deleting {
                                    "Delete"
                                } else {
                                    "Replace and play"
                                },
                                cx,
                                move |this, window, cx| {
                                    this.dismiss_playlist_action(window, cx);
                                    if deleting {
                                        if this.browser.playlists.selected() == Some(target.id) {
                                            this.browser.remove_playlist_selection();
                                        }
                                        this.send(
                                            Command::DeletePlaylist {
                                                playlist_id: target.id,
                                            },
                                            cx,
                                        );
                                    } else {
                                        this.send(
                                            Command::PlayPlaylist {
                                                playlist_id: target.id,
                                            },
                                            cx,
                                        );
                                    }
                                },
                            ))),
                    ),
            )
            .into_any_element()
    }

    fn empty_playlist_view(
        &self,
        kind: ViewRequestKind,
        panel_id: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let rows_error = if kind == ViewRequestKind::Entries {
            self.browser.playlists.entries().rows.error(0)
        } else {
            self.browser.playlists.summaries().rows.error(0)
        };
        let pending = self.browser.requests.pending(kind);
        let playlist_id = self.browser.playlists.selected();
        let message = if pending {
            "Loading…"
        } else if let Some(error) = rows_error {
            error
        } else if kind == ViewRequestKind::Entries {
            "This playlist is empty"
        } else {
            "No playlists yet"
        };
        let message: SharedString = message.to_owned().into();
        row()
            .p_2()
            .h(px(TRACK_HEIGHT))
            .w_full()
            .overflow_hidden()
            .min_h_0()
            .child(
                status_message(
                    message.clone(),
                    if rows_error.is_some() && !pending {
                        StatusKind::Error
                    } else {
                        StatusKind::Info
                    },
                )
                .id((
                    if kind == ViewRequestKind::Entries {
                        "playlist-entries-empty-status"
                    } else {
                        "playlist-catalog-empty-status"
                    },
                    panel_id,
                ))
                .flex_1()
                .min_w_0()
                .truncate()
                .tooltip(move |_, cx| {
                    cx.new(|_| super::ButtonTooltip {
                        text: message.clone(),
                    })
                    .into()
                }),
            )
            .when(rows_error.is_some() && !pending, |view| {
                view.child(icon_button(
                    (
                        if kind == ViewRequestKind::Entries {
                            "playlist-entries-empty-retry"
                        } else {
                            "playlist-catalog-empty-retry"
                        },
                        panel_id,
                    ),
                    "󰑐",
                    "Retry",
                    cx,
                    move |this, _, cx| {
                        if this.browser.requests.pending(kind) {
                            return;
                        }
                        if kind == ViewRequestKind::Entries {
                            if this.browser.playlists.selected() == playlist_id
                                && this.browser.playlists.entries().rows.error(0).is_some()
                                && let Some(id) = playlist_id
                            {
                                this.request_playlist_entries(id, 0, cx);
                            }
                        } else if this.browser.playlists.summaries().rows.error(0).is_some() {
                            this.request_playlist_summaries(0, cx);
                        }
                    },
                ))
            })
    }

    pub(super) fn playlists_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let playlist_id = self.browser.playlists.selected();
        let selection_entry = self.browser.playlists.selected_entry();
        let selection_unavailable = playlist_id.is_some()
            && selection_entry.is_some()
            && !self.browser.playlists.entry_ready();
        let selection_pending = self
            .browser
            .requests
            .pending(ViewRequestKind::EntrySelection);
        let selection_message = if !selection_unavailable {
            ""
        } else if selection_pending {
            "Refreshing selection…"
        } else {
            "Selection unavailable"
        };
        let selection_status = row()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(
                status_message(selection_message, StatusKind::Info)
                    .id(("playlist-selection-status", panel_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::ButtonTooltip {
                            text: selection_message.into(),
                        })
                        .into()
                    }),
            )
            .child(div().w(gpui::rems(4.)).h_full().flex_shrink_0().when(
                selection_unavailable && !selection_pending,
                |slot| {
                    slot.child(icon_button(
                        ("retry-playlist-entry", panel_id),
                        "󰑐",
                        "Retry",
                        cx,
                        move |this, _, cx| {
                            this.refresh(cx);
                            if this.browser.playlists.selected() == playlist_id
                                && this.browser.playlists.selected_entry() == selection_entry
                                && !this.browser.playlists.entry_ready()
                                && !this
                                    .browser
                                    .requests
                                    .pending(ViewRequestKind::EntrySelection)
                            {
                                this.request_entry_selection(cx);
                                cx.notify();
                            }
                        },
                    ))
                },
            ));
        let catalog_len = self.browser.playlists.summaries().rows.len();
        self.scroll_views.register_list(
            ("playlist-catalog", panel_id),
            &self.browser.playlists.summaries().scroll,
        );
        let catalog = uniform_list(
            ("playlist-catalog", panel_id),
            catalog_len,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| {
                        let Some(p) = this.browser.playlists.summaries().rows.get(index).cloned()
                        else {
                            return this
                                .paged_loading_row(ViewRequestKind::Summaries, index, cx)
                                .h(px(TRACK_HEIGHT))
                                .into_any_element();
                        };
                        let id = p.id;
                        let name = p.name.clone();
                        let play_target = PlaylistTarget {
                            id,
                            name: p.name.clone(),
                        };
                        let menu_target = play_target.clone();
                        let playable = p.entry_count > 0;
                        list_row(
                            ("playlist", id as u64),
                            this.browser.playlists.selected() == Some(id),
                        )
                        .group("playlist-catalog-row")
                        .child(row_text(
                            ("playlist-text", id as u64),
                            p.name,
                            format!("{} tracks", p.entry_count),
                        ))
                        .child(
                            quiet_icon_button(icon_button(
                                ("playlist-row-play", id as u64),
                                "󰐊",
                                "Play playlist",
                                cx,
                                move |this, window, cx| {
                                    if !playable {
                                        return;
                                    }
                                    this.refresh(cx);
                                    if this.state.queue.total == 0 {
                                        this.send(Command::PlayPlaylist { playlist_id: id }, cx);
                                    } else {
                                        this.focus_layout(window, cx);
                                        this.playlist_action =
                                            Some(PlaylistAction::Play(play_target.clone()));
                                        cx.notify();
                                    }
                                },
                            ))
                            .opacity(0.)
                            .group_hover("playlist-catalog-row", move |style| {
                                style.opacity(if playable { 1. } else { 0.4 })
                            })
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            }),
                        )
                        .child(
                            quiet_icon_button(icon_button(
                                ("playlist-row-menu", id as u64),
                                "󰇙",
                                "Playlist actions",
                                cx,
                                move |this, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.playlist_action = Some(PlaylistAction::Menu(
                                        menu_target.clone(),
                                        window.mouse_position(),
                                    ));
                                    cx.notify();
                                },
                            ))
                            .opacity(0.)
                            .group_hover("playlist-catalog-row", |style| style.opacity(1.))
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            }),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.focus_layout(window, cx);
                            this.browser.focus_playlists();
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
        .with_decoration(paged::viewport_demand(
            cx.processor(|this, range, _, cx| this.ensure_playlist_range(range, cx)),
        ))
        .h_full()
        .min_h_0()
        .w_full()
        .track_scroll(&self.browser.playlists.summaries().scroll);
        let catalog = if catalog_len == 0 {
            self.empty_playlist_view(ViewRequestKind::Summaries, panel_id, cx)
                .into_any_element()
        } else {
            catalog.into_any_element()
        };
        let mut lists = column()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(list_viewport(catalog).when(playlist_id.is_some(), |view| {
                view.flex_none()
                    .h(px(catalog_len.max(1) as f32 * TRACK_HEIGHT + 2.))
                    .max_h(gpui::relative(1. / 3.))
            }));
        if let Some(playlist_id) = playlist_id {
            self.scroll_views.register_list(
                ("playlist-entries", panel_id),
                &self.browser.playlists.entries().scroll,
            );
            let entries_len = self.browser.playlists.entries().rows.len();
            let entries = uniform_list(
                ("playlist-entries", panel_id),
                entries_len,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|index| {
                            let Some(e) = this.browser.playlists.entries().rows.get(index).cloned()
                            else {
                                return this
                                    .paged_loading_row(ViewRequestKind::Entries, index, cx)
                                    .h(px(TRACK_HEIGHT))
                                    .into_any_element();
                            };
                            let id = e.id;
                            let track_id = e.track_id;
                            let revision = this.playlist_revision(playlist_id);
                            let catalog_revision = this.state.library.playlist_revision;
                            let selection_generation =
                                this.browser.playlists.selection_generation();
                            let drag = PlaylistDrag {
                                playlist_id,
                                entry_id: id,
                                position: index,
                                revision,
                                catalog_revision,
                                selection_generation,
                            };
                            let detail = format!(
                                "{}{}{}",
                                e.artist,
                                if e.artist.is_empty() || e.album.is_empty() {
                                    ""
                                } else {
                                    " / "
                                },
                                e.album
                            );
                            track_list_row(
                                ("playlist-entry", id as u64),
                                ("playlist-entry-text", id as u64),
                                e.title,
                                detail,
                                index,
                                false,
                                this.browser.playlists.selected_entry() == Some(id),
                            )
                            .group("playlist-track-row")
                            .child(this.track_favorite_toggle(
                                ("playlist-entry-favorite", id as u64),
                                "playlist-track-row",
                                track_id,
                                e.favorite,
                                cx,
                            ))
                            .child(
                                quiet_icon_button(icon_button(
                                    ("playlist-entry-remove", id as u64),
                                    "",
                                    "Remove from playlist",
                                    cx,
                                    move |this, _, cx| {
                                        this.refresh(cx);
                                        if this.browser.playlists.selected() != Some(playlist_id)
                                            || this.playlist_revision(playlist_id) != revision
                                            || this.state.library.playlist_revision
                                                != catalog_revision
                                        {
                                            return;
                                        }
                                        if this.browser.playlists.selected_entry() == Some(id) {
                                            this.browser.playlists.take_entry();
                                        }
                                        this.send(
                                            Command::RemovePlaylistEntry { entry_id: id },
                                            cx,
                                        );
                                    },
                                ))
                                .opacity(0.)
                                .group_hover("playlist-track-row", |style| style.opacity(1.))
                                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                }),
                            )
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.browser.focus_playlists();
                                    this.browser
                                        .requests
                                        .cancel(ViewRequestKind::EntrySelection);
                                    this.browser.playlists.select_entry(id, index);
                                    this.request_track(track_id, cx);
                                    if event.click_count() == 2 {
                                        this.send(Command::Play { track_id }, cx);
                                    }
                                    cx.notify();
                                },
                            ))
                            .on_drag(drag, |drag, _, _, cx| cx.new(|_| *drag))
                            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                                window.prevent_default();
                                this.move_playlist_entry(drag, id, cx);
                            }))
                            .drag_over::<PlaylistDrag>(|style, _, _, _| style.bg(rgb(HIGHLIGHT)))
                            .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .with_decoration(paged::viewport_demand(
                cx.processor(|this, range, _, cx| this.ensure_entry_range(range, cx)),
            ))
            .h_full()
            .min_h_0()
            .w_full()
            .track_scroll(&self.browser.playlists.entries().scroll);
            let entries = if entries_len == 0 {
                self.empty_playlist_view(ViewRequestKind::Entries, panel_id, cx)
                    .into_any_element()
            } else {
                entries.into_any_element()
            };
            lists = lists.child(
                list_viewport(entries)
                    .flex_none()
                    .flex_shrink_1()
                    .h(px(entries_len.max(1) as f32 * TRACK_HEIGHT + 2.))
                    .max_h(gpui::relative(1.)),
            );
        }
        panel_surface(("playlists-panel", panel_id))
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
                format!("{} playlists", self.browser.playlists.summaries().total),
            ))
            .child(
                row()
                    .h(gpui::rems(2.))
                    .flex_shrink_0()
                    .child(icon_button(
                        ("playlist-create", panel_id),
                        "",
                        "New playlist…",
                        cx,
                        |this, window, cx| {
                            this.begin_playlist_name(
                                super::playlist_editor::PlaylistNameAction::Create {
                                    track_ids: Vec::new(),
                                },
                                String::new(),
                                window,
                                cx,
                            );
                        },
                    ))
                    .child(icon_button(
                        ("playlist-import", panel_id),
                        "",
                        "Import M3U",
                        cx,
                        |this, _, cx| this.choose_playlist_import(cx),
                    ))
                    .child(disabled_control(
                        icon_button(
                            ("playlist-export", panel_id),
                            "",
                            "Export playlist",
                            cx,
                            move |this, _, cx| {
                                if let Some(id) = playlist_id {
                                    this.choose_playlist_export(id, cx);
                                }
                            },
                        ),
                        playlist_id.is_none(),
                    ))
                    .child(selection_status),
            )
            .child(lists)
            .into_any_element()
    }

    pub(super) fn metadata_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.sync_metadata_inputs(cx);
        self.prepare_artwork(cx);
        let track = self.metadata.track();
        let id = track.map(|track| track.id);
        let favorite = track.is_some_and(|track| track.favorite);
        let ready = self.metadata.ready();
        let metadata_dirty = ready && self.metadata_dirty(cx) && !self.metadata.saving();
        let selected = self.metadata.selected();
        let can_retry = selected.is_some()
            && matches!(
                self.metadata.load_state(),
                super::metadata::MetadataLoadState::Failed(_)
            );
        let placeholder: SharedString = "Not loaded".into();
        let status: SharedString = match self.metadata.load_state() {
            super::metadata::MetadataLoadState::Empty => "No track selected".into(),
            super::metadata::MetadataLoadState::Loading => "Loading track details…".into(),
            super::metadata::MetadataLoadState::Ready => "Ready".into(),
            super::metadata::MetadataLoadState::Unavailable => "Track unavailable".into(),
            super::metadata::MetadataLoadState::Failed(error) => {
                format!("Could not load track details: {error}").into()
            }
        };
        let heading = self
            .metadata
            .selected()
            .or(id)
            .map_or_else(|| "Track details".to_owned(), |id| format!("Track #{id}"));
        let file: [[(&'static str, SharedString); 2]; 1] = [[
            (
                "File path",
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        if track.missing {
                            "Missing".into()
                        } else {
                            track.path.to_string_lossy().into_owned().into()
                        }
                    },
                ),
            ),
            (
                "Availability",
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        if track.missing {
                            "Missing".into()
                        } else {
                            "Available".into()
                        }
                    },
                ),
            ),
        ]];
        let metadata: [[(&'static str, SharedString); 2]; 3] = [
            [
                (
                    "Codec",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            if track.codec.is_empty() {
                                "—".into()
                            } else {
                                track.codec.clone().into()
                            }
                        },
                    ),
                ),
                (
                    "Sample rate",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            if track.sample_rate == 0 {
                                "—".into()
                            } else {
                                format!("{} Hz", track.sample_rate).into()
                            }
                        },
                    ),
                ),
            ],
            [
                (
                    "Channels",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            if track.channels == 0 {
                                "—".into()
                            } else {
                                track.channels.to_string().into()
                            }
                        },
                    ),
                ),
                (
                    "Duration",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .duration
                                .map_or("—".into(), |value| format_time(value).into())
                        },
                    ),
                ),
            ],
            [
                (
                    "Bitrate",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track.bitrate_bps.map_or("—".into(), |value| {
                                format!("{:.1} kbps", value as f64 / 1000.0).into()
                            })
                        },
                    ),
                ),
                (
                    "Bits/sample",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .bits_per_sample
                                .map_or("—".into(), |value| value.to_string().into())
                        },
                    ),
                ),
            ],
        ];
        let disc: (&'static str, SharedString) = (
            "Disc",
            track.map_or_else(
                || placeholder.clone(),
                |track| {
                    track
                        .disc_number
                        .map_or("—".into(), |value| value.to_string().into())
                },
            ),
        );
        let tags: [[(&'static str, SharedString); 2]; 2] = [
            [
                (
                    "Track",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .track_number
                                .map_or("—".into(), |value| value.to_string().into())
                        },
                    ),
                ),
                (
                    "Release",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .release_date
                                .as_deref()
                                .unwrap_or("—")
                                .to_owned()
                                .into()
                        },
                    ),
                ),
            ],
            [
                (
                    "Play count",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| track.play_count.to_string().into(),
                    ),
                ),
                (
                    "Last played",
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            let text = self.last_played_text(track.id, track.last_played);
                            text.strip_prefix("Last played · ")
                                .unwrap_or(&text)
                                .to_owned()
                                .into()
                        },
                    ),
                ),
            ],
        ];
        // Static labels are measured even before values load, using the caption font.
        // Every card shares the same two column widths.
        let label_widths = std::array::from_fn(|index| {
            file.iter()
                .chain(&metadata)
                .chain(&tags)
                .map(|fields| fields[index].0)
                .chain((index == 1).then_some(disc.0))
                .map(|label| {
                    let run = gpui::TextRun {
                        len: label.len(),
                        font: self.ui_font.clone(),
                        color: rgb(MUTED).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    window
                        .text_system()
                        .shape_line(label.into(), window.rem_size() * 0.75, &[run], None)
                        .width()
                        .ceil()
                })
                .fold(px(0.), gpui::Pixels::max)
        });
        let scroll = self.scroll_views.content(("metadata", panel_id));
        let details = div()
            .id(("metadata-details", panel_id))
            .flex().flex_col().flex_1().min_h_0()
            .overflow_y_scroll().track_scroll(&scroll)
            .child(
                column().flex_shrink_0().p(px(UI_INSET)).gap_3()
                    .child(
                        column().gap_1()
                            .child(
                                row().justify_between().items_center()
                                    .child(caption(heading).text_lg().text_color(rgb(TEXT)))
                                    .child(
                                        row().gap_2()
                                            .child(
                                                disabled_control(icon_button(
                                                    ("metadata-favorite", panel_id),
                                                    if favorite { "󰓎" } else { "󰓐" },
                                                    if favorite { "Remove from favorites" } else { "Add to favorites" },
                                                    cx,
                                                    move |this, _, cx| {
                                                        let Some((track_id, favorite)) = this.metadata.track()
                                                            .filter(|track| Some(track.id) == id)
                                                            .map(|track| (track.id, track.favorite)) else {
                                                            return;
                                                        };
                                                        this.send(Command::SetFavorite {
                                                            track_ids: vec![track_id], favorite: !favorite,
                                                        }, cx);
                                                    },
                                                ).text_color(rgb(if favorite { ACCENT } else { TEXT })), !ready),
                                            )
                                            .child(
                                                disabled_control(icon_button(
                                                    ("metadata-save", panel_id), "󰆓",
                                                    "Save metadata. Media files are never modified.", cx,
                                                    move |this, _, cx| {
                                                        if this.metadata.track().is_some_and(|track| Some(track.id) == id) {
                                                            this.save_metadata(cx);
                                                        }
                                                    },
                                                ), !metadata_dirty),
                                            )
                                            .child(
                                                disabled_control(icon_button(("metadata-retry", panel_id), "󰑐", "Retry loading track details", cx, move |this, _, cx| {
                                                    if this.metadata.selected() == selected
                                                        && matches!(this.metadata.load_state(), super::metadata::MetadataLoadState::Failed(_))
                                                        && let Some(id) = selected
                                                    {
                                                        this.request_track(id, cx);
                                                        cx.notify();
                                                    }
                                                }), !can_retry),
                                            ),
                                    ),
                            )
                            .child(
                                div().id(("metadata-status", panel_id)).min_w_0()
                                    .h(gpui::rems(1.25))
                                    .child(caption(status.clone()).text_xs().text_color(rgb(MUTED)).truncate())
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| super::ButtonTooltip { text: status.clone() }).into()
                                    }),
                            ),
                    )
                    .child(metadata_group("File", metadata_rows(file, label_widths)))
                    .child(metadata_group("Metadata", metadata_rows(metadata, label_widths)))
                    .child(metadata_group(
                        "Decoder hint",
                        column().gap_2()
                            .child(row().flex_wrap().children(
                                [
                                    ("metadata-decoder-auto", "Auto", DecoderHint::Auto),
                                    ("metadata-decoder-native", "Native", DecoderHint::Native),
                                    ("metadata-decoder-ffmpeg", "FFmpeg", DecoderHint::Ffmpeg),
                                ].into_iter().map(|(key, label, decoder_hint)| {
                                    disabled_control(active_control(
                                        button((key, panel_id), label, cx, move |this, _, cx| {
                                            if this.metadata.track().is_some_and(|track| Some(track.id) == id) {
                                                this.metadata_decoder_hint = decoder_hint;
                                                cx.notify();
                                            }
                                        }),
                                        ready && self.metadata_decoder_hint == decoder_hint,
                                    ), !ready)
                                }),
                            ))
                            .child(caption(
                                "Use Save to apply metadata and decoder hint changes together. The saved hint applies on the next track open or full waveform request.",
                            ).text_xs().text_color(rgb(MUTED)))
                            .child(caption(
                                "FFmpeg requires the extension to be enabled. Native may play DTS-WAV as noise.",
                            ).text_xs().text_color(rgb(MUTED))),
                    ))
                    .child(metadata_group(
                        "Tags",
                        column().gap_2()
                            .child(metadata_row(self.panel_field(Field::Title, "Title"), self.panel_field(Field::Artist, "Artist")))
                            .child(metadata_row(self.panel_field(Field::Album, "Album"), metadata_item(disc.0, disc.1, label_widths[1])))
                            .child(metadata_rows(tags, label_widths).gap_2())
                            .when_some(self.artwork_view.image(cx), |tags, image| {
                                tags.child(artwork_preview(image, self.artwork_view.aspect_ratio(cx)))
                            }),
                    )),
            );
        panel_surface(("metadata-panel", panel_id))
            .child(details)
            .into_any_element()
    }

    fn history_sort_button(
        &self,
        panel_id: u64,
        label: &'static str,
        sort: HistorySort,
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
            None,
            cx,
            move |this, _, cx| {
                this.browser.history.set_sort(sort);
                cx.notify();
            },
        )
        .pr_2()
    }

    pub(super) fn history_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let history_len = self.browser.history.len();
        let header =
            row()
                .items_center()
                .gap_0()
                .px_2()
                .h(gpui::rems(2.25))
                .flex_shrink_0()
                .min_w_0()
                .overflow_hidden()
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(caption("#").w(gpui::relative(0.08)).pr_2().flex_shrink_0())
                .child(self.history_sort_button(panel_id, "Track", HistorySort::Title, cx))
                .child(div().w(gpui::relative(0.2)).h_full().flex_shrink_0().child(
                    self.history_sort_button(panel_id, "Play count", HistorySort::Plays, cx),
                ))
                .child(div().w(gpui::relative(0.3)).h_full().flex_shrink_0().child(
                    self.history_sort_button(panel_id, "Last played", HistorySort::Recent, cx),
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
                                    let item = this.browser.history.get(index)?;
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
                                        .gap_0()
                                        .child(
                                            caption((index + 1).to_string())
                                                .w(gpui::relative(0.08))
                                                .pr_2()
                                                .flex_shrink_0(),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .pr_2()
                                                .text_sm()
                                                .truncate()
                                                .child(title),
                                        )
                                        .child(
                                            caption(item.play_count.to_string())
                                                .w(gpui::relative(0.2))
                                                .pr_2()
                                                .flex_shrink_0()
                                                .truncate(),
                                        )
                                        .child(
                                            caption(last_played.to_owned())
                                                .w(gpui::relative(0.3))
                                                .pr_2()
                                                .flex_shrink_0()
                                                .truncate(),
                                        )
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, window, cx| {
                                                this.focus_layout(window, cx);
                                                this.browser.focus_history();
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
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .track_scroll(&scroll),
                ),
            ))
            .into_any_element()
    }
}
