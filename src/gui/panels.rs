use super::{
    ACCENT, BORDER, Dragging, Field, GuiApp, HIGHLIGHT, HistorySort, ListFocus, MUTED, Measured,
    StatusKind, TEXT, UI_INSET, ViewRequestKind, active_control, artwork_preview, button, column,
    components::{
        LIST_ROW_REMS, danger_control, disabled_control, list_row_height, quiet_icon_button,
        scrollable_list, track_list_row,
    },
    form_field, format_time, icon_button,
    library::LibraryDrag,
    list_viewport,
    metadata::session::{MetadataAvailability, MetadataDraft, MetadataReadState},
    metadata_item, paged, panel_header, panel_surface, panel_toolbar, row, row_text, section_group,
    sortable_header, status_message,
};
pub(super) use super::{caption, list_row};
use crate::{
    gui::QueueDrag,
    model::{Command, DecoderHint, PlaybackStatus, RepeatMode},
};
use chrono::{DateTime, Local};
use gpui::{
    AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb, uniform_list,
};

// Re-measure only when content, playback labels, font, or UI scale changes.
// Layout growth belongs exclusively to Track, not these three content columns.
pub(super) struct HistoryColumnWidths {
    source: std::sync::Arc<Vec<crate::model::HistoryEntry>>,
    font: gpui::Font,
    rem_size: gpui::Pixels,
    playback: (Option<i64>, PlaybackStatus, Option<i64>),
    widths: [gpui::Pixels; 3],
}

fn compact_icon_button(control: gpui::Stateful<Div>) -> gpui::Stateful<Div> {
    quiet_icon_button(control).w(gpui::rems(1.4))
}

// Out-of-flow actions leave the full text width available until the row is hovered.
pub(super) fn hover_actions(group: &'static str, selected: bool) -> Div {
    row()
        .absolute()
        .right_0()
        .top_0()
        .h_full()
        .gap_0()
        .bg(super::components::row_background(selected, true))
        .invisible()
        .group_hover(group, |style| style.visible())
}

fn metadata_rows(
    rows: impl IntoIterator<Item = [(SharedString, SharedString); 2]>,
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

fn metadata_group(title: impl Into<SharedString>, content: impl IntoElement) -> Div {
    section_group(title, content)
}

pub(super) struct PlaylistDrag {
    i18n: std::sync::Arc<crate::i18n::Localizer>,
    playlist_id: i64,
    source_id: i64,
    // GPUI keeps the original on_drag value; the constructor creates only its preview.
    entry_ids: std::cell::OnceCell<Vec<i64>>,
    revision: u64,
    stamp: crate::model::QueryStamp,
    selection_generation: u64,
}

impl PlaylistDrag {
    fn accepts(&self, target: Option<i64>) -> bool {
        self.entry_ids
            .get()
            .is_some_and(|ids| !ids.is_empty() && target.is_none_or(|id| !ids.contains(&id)))
    }

    fn command(&self, target: Option<i64>) -> Option<Command> {
        if !self.accepts(target) {
            return None;
        }
        let entry_ids = self.entry_ids.get()?;
        Some(Command::MovePlaylistEntries {
            playlist_id: self.playlist_id,
            entry_ids: entry_ids.clone(),
            before_entry_id: target,
            expected_revision: self.revision,
        })
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
            return self.i18n.text("gl-played-now").into();
        }

        let played_at = if current && self.state.playback.status == PlaybackStatus::Paused {
            self.state.playback.last_heard_at
        } else {
            stored_at
        };
        match played_at.and_then(|time| DateTime::from_timestamp(time, 0)) {
            Some(time) => self.i18n.format(
                "gl-played-at",
                &[
                    (
                        "time",
                        (time
                            .with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string())
                        .into(),
                    ),
                    (
                        "paused",
                        (usize::from(
                            current && self.state.playback.status == PlaybackStatus::Paused,
                        ))
                        .into(),
                    ),
                ],
            ),
            None => self.i18n.text("gl-played-none").into(),
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
            .unwrap_or_else(|| self.i18n.text("gl-choose-track").into());
        let artist = track.map(|track| track.artist.clone()).unwrap_or_default();
        let status = match self.state.playback.status {
            PlaybackStatus::Playing => self.tr("gl-playing"),
            PlaybackStatus::Paused => self.tr("gl-paused"),
            PlaybackStatus::Stopped => self.tr("gl-stopped"),
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
            .flex_shrink_0()
            .justify_center()
            .child(active_control(
                quiet_icon_button(icon_button(
                    ("shuffle", panel_id),
                    "󰒟",
                    if self.state.playback.shuffle {
                        self.tr("gl-shuffle-on")
                    } else {
                        self.tr("gl-shuffle-off")
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
                    self.tr("gl-previous-track"),
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
                        self.tr("gl-pause")
                    } else {
                        self.tr("gl-play")
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
                    self.tr("gl-next-track"),
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
                        RepeatMode::Off => self.tr("gl-repeat-off"),
                        RepeatMode::All => self.tr("gl-repeat-all"),
                        RepeatMode::One => self.tr("gl-repeat-one"),
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
                    .tooltip({
                        let text = self.tr("gl-volume");
                        move |_, cx| {
                            cx.new(|_| super::ButtonTooltip { text: text.clone() })
                                .into()
                        }
                    }),
            )
            .child(div().flex_1().min_w_0().child(volume))
            .child(
                caption(format!("{:.0}%", self.state.playback.volume * 100.))
                    .w(gpui::rems(3.5))
                    .whitespace_nowrap()
                    .flex_shrink_0(),
            );
        let scroll = self.scroll_views.content(("transport", panel_id));
        column()
            .id(("transport-panel", panel_id))
            .size_full()
            .min_w_0()
            .min_h_0()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .child(
                row()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .child(div().flex_1().min_w_0().text_lg().truncate().child(title))
                    .child(disabled_control(
                        quiet_icon_button(icon_button(
                            ("playback-locate", panel_id),
                            "",
                            self.tr("gl-locate-playing-entry"),
                            cx,
                            |this, window, cx| {
                                if this.state.queue.current_id.is_some()
                                    && this.reveal_queue_panel(window, cx)
                                {
                                    this.locate_playing_queue(window, cx);
                                }
                            },
                        )),
                        self.state.queue.current_id.is_none(),
                    ))
                    .child(caption(status).flex_shrink_0()),
            )
            .child(
                row()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .child(caption(artist).min_w_0().truncate())
                    .when_some(
                        track.filter(|track| !track.album.is_empty()),
                        |view, track| view.child(caption(track.album.clone()).min_w_0().truncate()),
                    ),
            )
            .child(
                row()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .flex_wrap()
                    .child(div().flex_1().min_w(gpui::rems(4.)).child(seek))
                    .child(
                        caption(format!(
                            "{} / {}",
                            format_time(preview_position),
                            duration.map_or("—".into(), format_time)
                        ))
                        .whitespace_nowrap()
                        .flex_shrink_0(),
                    ),
            )
            .child(
                row()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .id(("transport-controls", panel_id))
                    .overflow_x_scroll()
                    .justify_between()
                    .child(controls)
                    .child(volume_control),
            )
            .into_any_element()
    }
    fn panel_field(&self, field: Field, label: impl Into<SharedString>) -> Div {
        form_field(label, self.input(field))
    }

    pub(super) fn panel_selected_tracks(&self) -> Vec<i64> {
        if self.library_range_pending() {
            return Vec::new();
        }
        let mut tracks: Vec<_> = self.browser.library.selected().iter().copied().collect();
        // Match library drag ordering instead of exposing HashSet iteration order.
        tracks.sort_unstable();
        tracks
    }
    pub(super) fn panel_track_text(&self, track: &crate::model::LibraryRow) -> (String, String) {
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
                self.i18n.text("gl-file-missing-suffix")
            } else {
                ""
            }
        );
        (track.title.clone(), detail)
    }

    fn library_scan_status(&self, panel_id: u64, cx: &mut Context<Self>) -> Div {
        use crate::model::ScanPhase;
        let progress = self.state.system.scan_progress.as_ref();
        let header_text = progress.map_or_else(
            || self.i18n.text("gl-scanning").to_owned(),
            |progress| {
                let phase = match progress.phase {
                    ScanPhase::Discovering => self.i18n.text("gl-scan-discovering"),
                    ScanPhase::ReadingMetadata => self.i18n.text("gl-scan-metadata"),
                    ScanPhase::Saving => self.i18n.text("gl-scan-saving"),
                    ScanPhase::Completed => self.i18n.text("gl-scan-completed"),
                    ScanPhase::Failed => self.i18n.text("gl-scan-failed"),
                    ScanPhase::Cancelled => self.i18n.text("gl-scan-cancelled"),
                };
                match progress.total {
                    Some(total) => self.i18n.format(
                        "gl-scan-progress",
                        &[
                            ("phase", (phase).into()),
                            ("processed", (progress.processed).into()),
                            ("total", (total).into()),
                            ("errors", (progress.errors).into()),
                        ],
                    ),
                    None => self.i18n.format(
                        "gl-scan-found",
                        &[
                            ("phase", (phase).into()),
                            ("processed", (progress.processed).into()),
                            ("errors", (progress.errors).into()),
                        ],
                    ),
                }
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
                    self.tr("gl-dismiss-scan"),
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
        compact_icon_button(icon_button(
            id,
            if favorite { "󰓎" } else { "󰓐" },
            if favorite {
                self.tr("gl-unfavorite")
            } else {
                self.tr("gl-favorite")
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.finish_library_locate_render(window, cx);
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
            self.i18n.text("gl-selecting").to_owned()
        } else if selected_directories > 0 && self.browser.library.selected().is_empty() {
            self.i18n.format(
                "gl-folders-selected",
                &[("count", (selected_directories).into())],
            )
        } else if selected_directories > 0 {
            self.i18n.format(
                "gl-mixed-selected",
                &[
                    ("tracks", (self.browser.library.selected().len()).into()),
                    ("folders", (selected_directories).into()),
                ],
            )
        } else if !self.browser.library.selected().is_empty() {
            self.i18n.format(
                "gl-tracks-selected",
                &[("count", (self.browser.library.selected().len()).into())],
            )
        } else {
            self.i18n
                .format("gl-entry-count", &[("count", (total).into())])
        };
        let mut tools = panel_toolbar();
        tools = tools
            .when(!tab_bar_visible, |tools| {
                tools.child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(self.tr("gl-library")),
                )
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
            .child(disabled_control(
                icon_button(
                    ("library-locate-playing", panel_id),
                    "",
                    self.tr("gl-locate-library"),
                    cx,
                    |this, window, cx| this.locate_playing_library(window, cx),
                ),
                self.state.current_track().is_none() || self.browser.library.locate.pending,
            ))
            .child(icon_button(
                ("library-view", panel_id),
                if self.browser.library.tree_active() {
                    "󰉋"
                } else {
                    "󰉢"
                },
                if self.browser.library.tree_active() {
                    self.tr("gl-flat-view")
                } else {
                    self.tr("gl-tree-view")
                },
                cx,
                |this, _, cx| this.toggle_library_view(cx),
            ))
            .child(active_control(
                icon_button(
                    ("library-filter-favorites", panel_id),
                    "󰓎",
                    self.tr("gl-filter-favorites"),
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
                    self.tr("gl-filter-missing"),
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
                self.tr("gl-add-root"),
                cx,
                |this, _, cx| this.choose_library_root(cx),
            ))
            .child(active_control(
                icon_button(
                    ("library-force-scan", panel_id),
                    "󰑐",
                    if self.browser.library.force_scan() {
                        self.tr("gl-force-on")
                    } else {
                        self.tr("gl-force-off")
                    },
                    cx,
                    |this, _, cx| {
                        this.browser.library.toggle_force_scan();
                        cx.notify();
                    },
                ),
                self.browser.library.force_scan(),
            ));
        let mut panel = panel_surface(("library-panel", panel_id))
            .child(tools)
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .child(self.input(Field::Search)),
            );
        if let Some((track_id, result)) = &self.browser.library.locate.notice
            && self
                .state
                .current_track()
                .is_some_and(|track| track.id == *track_id)
        {
            use crate::model::LibraryLocateResult;
            let message = match result {
                LibraryLocateResult::Filtered => Some("gl-locate-library-filtered"),
                LibraryLocateResult::OutsideRoot => Some("gl-locate-library-outside"),
                LibraryLocateResult::Unavailable => Some("gl-locate-library-unavailable"),
                LibraryLocateResult::Located(_) => None,
            };
            if let Some(message) = message {
                panel = panel.child(
                    row()
                        .flex_shrink_0()
                        .flex_wrap()
                        .child(caption(self.tr(message)))
                        .when(matches!(result, LibraryLocateResult::Filtered), |view| {
                            view.child(button(
                                ("library-clear-filters-locate", panel_id),
                                self.tr("gl-locate-clear-filters"),
                                cx,
                                |this, window, cx| {
                                    this.clear_filters_and_locate_library(window, cx)
                                },
                            ))
                        }),
                );
            }
        }
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

    fn queue_drop_index(
        &self,
        drag: &QueueDrag,
        target_id: Option<u64>,
        owner: gpui::EntityId,
    ) -> Option<usize> {
        if drag.owner != owner || drag.revision != self.state.queue.revision {
            return None;
        }
        let target = match target_id {
            Some(id) => Some(
                self.browser
                    .queue
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .map(super::queue::QueueIdentity::from)?,
            ),
            None => None,
        };
        drag.insertion_index(target, self.state.queue.total)
    }

    fn move_queue_entry(
        &mut self,
        drag: &QueueDrag,
        target_id: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        let Some(index) = self.queue_drop_index(drag, target_id, cx.entity().entity_id()) else {
            return;
        };
        self.send(
            Command::MoveQueueEntries {
                queue_ids: drag
                    .entries
                    .get()
                    .unwrap()
                    .iter()
                    .map(|row| row.id)
                    .collect(),
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
                header.child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(self.tr("gl-queue")),
                )
            })
            .child(
                caption(self.i18n.format(
                    "gl-queue-count",
                    &[
                        ("total", (self.state.queue.total).into()),
                        ("selected", (selection_count).into()),
                    ],
                ))
                .flex_shrink_0()
                .id(("queue-count", panel_id))
                .tooltip({
                    let text = self.tr("gl-queue-drag-hint");
                    move |_, cx| {
                        cx.new(|_| super::ButtonTooltip { text: text.clone() })
                            .into()
                    }
                }),
            );
        let mut tools = panel_toolbar().min_h(px(32.)).child(disabled_control(
            icon_button(
                ("queue-locate-playing", panel_id),
                "",
                self.tr("gl-locate-playing-entry"),
                cx,
                |this, window, cx| this.locate_playing_queue(window, cx),
            ),
            self.state.queue.current_id.is_none(),
        ));
        if selection_count > 1 {
            tools = tools.child(icon_button(
                ("queue-remove", panel_id),
                "",
                self.tr("gl-remove-queue-selected"),
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
        let (selection_message, selection_kind): (SharedString, _) =
            if let Some(error) = &self.browser.queue.selection_error {
                (error.clone().into(), StatusKind::Error)
            } else if self.browser.queue.busy {
                (
                    self.i18n.text("gl-selection-loading").into(),
                    StatusKind::Info,
                )
            } else {
                ("".into(), StatusKind::Info)
            };
        let status = row()
            .h(gpui::rems(2.))
            .flex_shrink_0()
            .overflow_hidden()
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
            .when(self.browser.queue.selection_error.is_some(), |status| {
                status.child(icon_button(
                    ("queue-selection-retry", panel_id),
                    "󰑐",
                    self.tr("gl-retry-selection"),
                    cx,
                    |this, _, cx| {
                        if this.browser.queue.selection_error.is_some() {
                            this.refresh_queue(cx);
                            cx.notify();
                        }
                    },
                ))
            });
        let scroll = self.scroll_views.list(("queue", panel_id));
        self.reveal_queue_locate(&scroll, cx);
        let panel_drop_owner = cx.entity().downgrade();
        let list_drop_owner = panel_drop_owner.clone();
        panel_surface(("queue-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &QueueDrag, window, cx| {
                window.prevent_default();
                this.move_queue_entry(drag, None, cx);
            }))
            .can_drop(move |value, _, cx| {
                panel_drop_owner.upgrade().is_some_and(|owner| {
                    let this = owner.read(cx);
                    if let Some(drag) = value.downcast_ref::<QueueDrag>() {
                        this.queue_drop_index(drag, None, owner.entity_id())
                            .is_some()
                    } else if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
                        this.can_resolve_library_drag(drag)
                    } else {
                        false
                    }
                })
            })
            .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                this.resolve_library_drag(drag.nodes(), super::DragDestination::Queue, cx);
            }))
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
            .child(header.child(tools))
            .when(
                self.browser.queue.busy || self.browser.queue.selection_error.is_some(),
                |panel| panel.child(status),
            )
            .child(list_viewport(scrollable_list(
                ("queue-scrollbar", panel_id),
                uniform_list(
                    ("queue-rows", panel_id),
                    self.state.queue.total,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let Some(entry) = this.browser.queue.rows.get(index) else {
                                    return this
                                        .paged_loading_row(ViewRequestKind::Queue, index, cx)
                                        .h(gpui::rems(LIST_ROW_REMS));
                                };
                                let id = entry.id;
                                let track_id = entry.track_id;
                                let favorite = entry.track.favorite;
                                let can_move_down = index + 1 < this.state.queue.total;
                                let revision = this.state.queue.revision;
                                let (title, detail) = this.panel_track_text(&entry.track);
                                let playing = this.state.queue.current_id == Some(id);
                                let drag_owner = cx.entity().downgrade();
                                let drop_owner = drag_owner.clone();
                                let drag_i18n = std::sync::Arc::clone(&this.i18n);
                                let drag = QueueDrag {
                                    owner: cx.entity().entity_id(),
                                    source: super::queue::QueueIdentity::from(entry),
                                    entries: Default::default(),
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
                                .when(this.queue_locate_emphasized(id), |row| {
                                    row.border_1().border_color(rgb(ACCENT)).bg(rgb(HIGHLIGHT))
                                })
                                .group("queue-track-row")
                                .relative()
                                .child(
                                    hover_actions(
                                        "queue-track-row",
                                        this.browser.queue.contains(&id),
                                    )
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(compact_icon_button(icon_button(
                                        ("queue-entry-play", id),
                                        "󰐊",
                                        this.tr("gl-play-queue-entry"),
                                        cx,
                                        move |this, _, cx| {
                                            this.send_queue_row_action(
                                                revision,
                                                Command::PlayQueue { queue_id: id },
                                                cx,
                                            )
                                        },
                                    )))
                                    .child(this.track_favorite_toggle(
                                        ("queue-entry-favorite", id),
                                        "queue-track-row",
                                        track_id,
                                        favorite,
                                        cx,
                                    ))
                                    .child(compact_icon_button(icon_button(
                                        ("queue-entry-remove", id),
                                        "",
                                        this.tr("gl-remove-queue"),
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
                                        compact_icon_button(icon_button(
                                            ("queue-entry-up", id),
                                            "",
                                            this.tr("gl-entry-up"),
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
                                        compact_icon_button(icon_button(
                                            ("queue-entry-down", id),
                                            "",
                                            this.tr("gl-entry-down"),
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
                                .when(
                                    this.browser.queue.ready()
                                        && this.browser.queue.selection_error.is_none(),
                                    |row| {
                                        row.on_drag(drag, move |drag, _, window, cx| {
                                            super::components::begin_list_drag(window, cx);
                                            drag.entries.get_or_init(|| {
                                                drag_owner
                                                    .update(cx, |this, cx| {
                                                        this.refresh(cx);
                                                        if drag.revision
                                                            != this.state.queue.revision
                                                            || !this.browser.queue.ready()
                                                            || this
                                                                .browser
                                                                .queue
                                                                .selection_error
                                                                .is_some()
                                                        {
                                                            return Vec::new();
                                                        }
                                                        if this
                                                            .browser
                                                            .queue
                                                            .contains(&drag.source.id)
                                                        {
                                                            this.browser.queue.ordered()
                                                        } else {
                                                            vec![drag.source]
                                                        }
                                                    })
                                                    .unwrap_or_default()
                                            });
                                            let label = drag_i18n
                                                .format(
                                                    "gl-move-queue-entries",
                                                    &[(
                                                        "count",
                                                        drag.entries
                                                            .get()
                                                            .map_or(0, Vec::len)
                                                            .into(),
                                                    )],
                                                )
                                                .into();
                                            cx.new(|_| super::components::ListDragPreview { label })
                                        })
                                    },
                                )
                                .on_drop(cx.listener(move |this, drag: &QueueDrag, window, cx| {
                                    window.prevent_default();
                                    cx.stop_propagation();
                                    this.move_queue_entry(drag, Some(id), cx);
                                }))
                                .can_drop(move |value, _, cx| {
                                    let Some(drag) = value.downcast_ref::<QueueDrag>() else {
                                        return false;
                                    };
                                    drop_owner.upgrade().is_some_and(|owner| {
                                        owner
                                            .read(cx)
                                            .queue_drop_index(drag, Some(id), owner.entity_id())
                                            .is_some()
                                    })
                                })
                                .drag_over::<QueueDrag>(|style, _, _, _| style.bg(rgb(HIGHLIGHT)))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .with_decoration(paged::viewport_demand(
                    cx.processor(|this, range, _, cx| this.ensure_queue_range(range, cx)),
                ))
                .on_drop(cx.listener(|this, drag: &QueueDrag, window, cx| {
                    window.prevent_default();
                    this.move_queue_entry(drag, None, cx);
                }))
                .can_drop(move |value, _, cx| {
                    list_drop_owner.upgrade().is_some_and(|owner| {
                        let this = owner.read(cx);
                        if let Some(drag) = value.downcast_ref::<QueueDrag>() {
                            this.queue_drop_index(drag, None, owner.entity_id())
                                .is_some()
                        } else if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
                            this.can_resolve_library_drag(drag)
                        } else {
                            false
                        }
                    })
                })
                .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
                .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                    window.prevent_default();
                    this.resolve_library_drag(drag.nodes(), super::DragDestination::Queue, cx);
                }))
                .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
                .h_full()
                .min_h_0()
                .w_full()
                .track_scroll(&scroll),
                &scroll,
            )))
            .into_any_element()
    }

    fn can_drop_playlist_entries(&self, drag: &PlaylistDrag, target: Option<i64>) -> bool {
        self.browser.playlists.selected() == Some(drag.playlist_id)
            && self.browser.playlists.entries().playlist_id == Some(drag.playlist_id)
            && self.browser.playlists.selection_generation() == drag.selection_generation
            && self.playlist_occurrence_stamp(drag.playlist_id) == drag.stamp
            && self.playlist_revision(drag.playlist_id) == drag.revision
            && drag.accepts(target)
    }

    fn move_playlist_entries(
        &mut self,
        drag: &PlaylistDrag,
        target_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.can_drop_playlist_entries(drag, target_id) {
            return;
        }
        if let Some(command) = drag.command(target_id) {
            self.send(command, cx);
        }
    }

    pub(super) fn dismiss_playlist_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.playlist_action = None;
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(super) fn playlist_action_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(action) = self.playlist_action.clone() else {
            return div().into_any_element();
        };
        if let PlaylistAction::Menu(target, position) = action {
            let id = target.id;
            let has_tracks = self.state.current_track().is_some();
            let rename = target.clone();
            let delete = target.clone();
            let menu = super::context_menu_container("playlist-row-menu")
                .max_w((window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.)))
                .max_h((window.viewport_size().height - px(UI_INSET * 2.)).max(px(0.)))
                .overflow_y_scroll()
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.dismiss_playlist_action(window, cx);
                    cx.stop_propagation();
                }))
                .child(
                    super::menu_item(
                        "playlist-row-add",
                        self.tr("gl-add-current"),
                        cx,
                        move |this, window, cx| {
                            let Some(track_id) = this.state.current_track().map(|track| track.id)
                            else {
                                return;
                            };
                            this.dismiss_playlist_action(window, cx);
                            this.send(
                                Command::AddPlaylist {
                                    playlist_id: id,
                                    track_ids: vec![track_id],
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
                    self.tr("gl-rename"),
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
                    self.tr("gl-delete"),
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
            self.i18n.format(
                "gl-confirm-delete-playlist",
                &[("name", (target.name.as_str()).into())],
            )
        } else {
            self.i18n.format(
                "gl-confirm-play-playlist",
                &[("name", (target.name.as_str()).into())],
            )
        };
        let detail = if deleting {
            self.i18n.text("gl-delete-playlist-note").to_owned()
        } else {
            self.i18n.format(
                "gl-replace-queue-confirm",
                &[("count", (self.state.queue.total).into())],
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
                    .id("playlist-action-card")
                    .w(gpui::rems(26.25))
                    .max_w(gpui::relative(0.9))
                    .max_h(window.viewport_size().height * 0.9)
                    .overflow_y_scroll()
                    .p(px(UI_INSET))
                    .bg(rgb(super::PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .child(panel_header(title, detail).flex_shrink_0())
                    .child(
                        row()
                            .flex_shrink_0()
                            .justify_end()
                            .child(icon_button(
                                "playlist-action-cancel",
                                "",
                                self.tr("gl-cancel"),
                                cx,
                                |this, window, cx| this.dismiss_playlist_action(window, cx),
                            ))
                            .child(danger_control(icon_button(
                                "playlist-action-confirm",
                                if deleting { "󰆴" } else { "󰐊" },
                                if deleting {
                                    self.tr("gl-delete-playlist")
                                } else {
                                    self.tr("gl-replace-playlist")
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
        let message = if let Some(error) = rows_error {
            error
        } else if pending {
            self.i18n.text("gl-loading")
        } else if kind == ViewRequestKind::Entries {
            self.i18n.text("gl-playlist-empty")
        } else {
            self.i18n.text("gl-no-playlists")
        };
        let message: SharedString = message.to_owned().into();
        row()
            .p_2()
            .h(gpui::rems(LIST_ROW_REMS))
            .flex_shrink_0()
            .w_full()
            .overflow_hidden()
            .min_h_0()
            .child(
                status_message(
                    message.clone(),
                    if rows_error.is_some() {
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
                .when(
                    pending
                        && !self.loading_text.show(
                            &super::loading::LoadingOwner::View(kind),
                            rows_error.is_some(),
                        ),
                    |text| text.invisible(),
                )
                .tooltip(move |_, cx| {
                    cx.new(|_| super::ButtonTooltip {
                        text: message.clone(),
                    })
                    .into()
                }),
            )
            .when(rows_error.is_some(), |view| {
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
                    self.tr("gl-retry"),
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = cx.weak_entity();
        window.defer(cx, move |_, cx| {
            let _ = owner.update(cx, |this, cx| this.request_playlist_selection(cx));
        });
        let row_height = list_row_height(window);
        let playlist_id = self.browser.playlists.selected();
        let selection_entry = self.browser.playlists.selected_entry();
        let selection_unavailable = !self.browser.playlists.track_selection.ready();
        let selection_pending = self.browser.playlists.track_selection.busy;
        let selection_message: SharedString = if !selection_unavailable {
            "".into()
        } else if selection_pending {
            self.tr("gl-selection-refreshing")
        } else {
            self.tr("gl-selection-unavailable")
        };
        let selection_status = row()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(
                status_message(selection_message.clone(), StatusKind::Info)
                    .id(("playlist-selection-status", panel_id))
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
            .child(div().w(gpui::rems(4.)).h_full().flex_shrink_0().when(
                selection_unavailable && !selection_pending,
                |slot| {
                    slot.child(icon_button(
                        ("retry-playlist-entry", panel_id),
                        "󰑐",
                        self.tr("gl-retry"),
                        cx,
                        move |this, _, cx| {
                            this.refresh(cx);
                            if this.browser.playlists.selected() == playlist_id
                                && this.browser.playlists.selected_entry() == selection_entry
                                && !this.browser.playlists.track_selection.ready()
                                && !this.browser.playlists.track_selection.busy
                            {
                                this.refresh_entry_selection(cx);
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
                        let Some(p) = this.browser.playlists.summaries().rows.get(index) else {
                            return this
                                .paged_loading_row(ViewRequestKind::Summaries, index, cx)
                                .h(gpui::rems(LIST_ROW_REMS))
                                .into_any_element();
                        };
                        let id = p.id;
                        let name = gpui::SharedString::from(p.name.clone());
                        let play_name = name.clone();
                        let menu_name = name.clone();
                        let context_name = name.clone();
                        let playable = p.entry_count > 0;
                        list_row(
                            ("playlist", id as u64),
                            this.browser.playlists.selected() == Some(id),
                        )
                        .group("playlist-catalog-row")
                        .gap_0()
                        .child(row_text(
                            ("playlist-text", id as u64),
                            name.clone(),
                            this.i18n
                                .format("gl-track-count", &[("count", (p.entry_count).into())]),
                        ))
                        .child(
                            compact_icon_button(icon_button(
                                ("playlist-row-play", id as u64),
                                "󰐊",
                                this.tr("gl-play-playlist"),
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
                                            Some(PlaylistAction::Play(PlaylistTarget {
                                                id,
                                                name: play_name.to_string(),
                                            }));
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
                            compact_icon_button(icon_button(
                                ("playlist-row-menu", id as u64),
                                "󰇙",
                                this.tr("gl-playlist-actions"),
                                cx,
                                move |this, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.playlist_action = Some(PlaylistAction::Menu(
                                        PlaylistTarget {
                                            id,
                                            name: menu_name.to_string(),
                                        },
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
                        .on_mouse_down(
                            gpui::MouseButton::Right,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                this.focus_layout(window, cx);
                                this.playlist_action = Some(PlaylistAction::Menu(
                                    PlaylistTarget {
                                        id,
                                        name: context_name.to_string(),
                                    },
                                    event.position,
                                ));
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.focus_layout(window, cx);
                            this.browser.focus_playlists();
                            this.select_playlist_container(id, name.to_string(), index, cx);
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
            scrollable_list(
                ("playlist-catalog-scrollbar", panel_id),
                catalog,
                &self.browser.playlists.summaries().scroll,
            )
            .into_any_element()
        };
        let mut lists = column()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(list_viewport(catalog).when(playlist_id.is_some(), |view| {
                view.flex_none()
                    .h(row_height * catalog_len.max(1) as f32 + px(2.))
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
                            let Some(e) = this.browser.playlists.entries().rows.get(index) else {
                                return this
                                    .paged_loading_row(ViewRequestKind::Entries, index, cx)
                                    .h(gpui::rems(LIST_ROW_REMS))
                                    .into_any_element();
                            };
                            let id = e.id;
                            let track_id = e.track_id;
                            let title = e.title.clone();
                            let favorite = e.favorite;
                            let revision = this.playlist_revision(playlist_id);
                            let stamp = this.playlist_occurrence_stamp(playlist_id);
                            let selection_generation =
                                this.browser.playlists.selection_generation();
                            let drag_owner = cx.entity().downgrade();
                            let drag = PlaylistDrag {
                                i18n: std::sync::Arc::clone(&this.i18n),
                                playlist_id,
                                source_id: id,
                                entry_ids: Default::default(),
                                revision,
                                stamp,
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
                                title,
                                detail,
                                index,
                                false,
                                this.browser.playlists.track_selection.contains(id),
                            )
                            .group("playlist-track-row")
                            .relative()
                            .gap_0()
                            .child(
                                hover_actions(
                                    "playlist-track-row",
                                    this.browser.playlists.track_selection.contains(id),
                                )
                                .child(
                                    compact_icon_button(icon_button(
                                        ("playlist-entry-play", id as u64),
                                        "󰐊",
                                        this.tr("gl-play-track"),
                                        cx,
                                        move |this, _, cx| {
                                            this.send(Command::Play { track_id }, cx)
                                        },
                                    ))
                                    .opacity(0.)
                                    .group_hover("playlist-track-row", |style| style.opacity(1.))
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    }),
                                )
                                .child(this.track_favorite_toggle(
                                    ("playlist-entry-favorite", id as u64),
                                    "playlist-track-row",
                                    track_id,
                                    favorite,
                                    cx,
                                ))
                                .child(
                                    compact_icon_button(icon_button(
                                        ("playlist-entry-remove", id as u64),
                                        "",
                                        this.tr("gl-remove-playlist"),
                                        cx,
                                        move |this, _, cx| {
                                            this.refresh(cx);
                                            if this.browser.playlists.selected()
                                                != Some(playlist_id)
                                                || this.playlist_occurrence_stamp(playlist_id)
                                                    != stamp
                                            {
                                                return;
                                            }
                                            this.send(
                                                Command::RemovePlaylistEntries {
                                                    playlist_id,
                                                    entry_ids: vec![id],
                                                    expected_revision: revision,
                                                },
                                                cx,
                                            );
                                        },
                                    ))
                                    .opacity(0.)
                                    .group_hover("playlist-track-row", |style| style.opacity(1.))
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    }),
                                ),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.browser.focus_list(ListFocus::PlaylistTracks);
                                }),
                            )
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.browser.focus_list(ListFocus::PlaylistTracks);
                                    let modifiers = event.modifiers();
                                    this.select_playlist_entry(
                                        id,
                                        index,
                                        modifiers.shift,
                                        modifiers.control || modifiers.platform,
                                        cx,
                                    );
                                    if event.click_count() == 2 {
                                        this.send(Command::Play { track_id }, cx);
                                    }
                                    cx.notify();
                                },
                            ))
                            .when(!selection_unavailable, |row| {
                                row.on_drag(drag, move |drag, _, window, cx| {
                                    super::components::begin_list_drag(window, cx);
                                    drag.entry_ids.get_or_init(|| {
                                        drag_owner
                                            .update(cx, |this, cx| {
                                                if !this
                                                    .browser
                                                    .playlists
                                                    .track_selection
                                                    .contains(drag.source_id)
                                                {
                                                    this.select_playlist_entry(
                                                        drag.source_id,
                                                        index,
                                                        false,
                                                        false,
                                                        cx,
                                                    );
                                                }
                                                this.browser
                                                    .playlists
                                                    .track_selection
                                                    .drag_ids(drag.source_id)
                                            })
                                            .unwrap_or_default()
                                    });
                                    let label = drag
                                        .i18n
                                        .format(
                                            "gl-move-playlist-entries",
                                            &[(
                                                "count",
                                                drag.entry_ids.get().map_or(0, Vec::len).into(),
                                            )],
                                        )
                                        .into();
                                    cx.new(|_| super::components::ListDragPreview { label })
                                })
                            })
                            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.move_playlist_entries(drag, Some(id), cx);
                            }))
                            .can_drop({
                                let owner = cx.weak_entity();
                                move |value, _, cx| {
                                    owner.upgrade().is_some_and(|owner| {
                                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                                            owner.read(cx).can_drop_playlist_entries(drag, Some(id))
                                        })
                                    })
                                }
                            })
                            .drag_over::<PlaylistDrag>(|style, _, _, _| style.bg(rgb(HIGHLIGHT)))
                            .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .with_decoration(paged::viewport_demand(
                cx.processor(|this, range, _, cx| this.ensure_entry_range(range, cx)),
            ))
            .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                this.move_playlist_entries(drag, None, cx);
            }))
            .can_drop({
                let owner = cx.weak_entity();
                move |value, _, cx| {
                    owner.upgrade().is_some_and(|owner| {
                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                            owner.read(cx).can_drop_playlist_entries(drag, None)
                        })
                    })
                }
            })
            .h_full()
            .min_h_0()
            .w_full()
            .track_scroll(&self.browser.playlists.entries().scroll);
            let entries = if entries_len == 0 {
                self.empty_playlist_view(ViewRequestKind::Entries, panel_id, cx)
                    .into_any_element()
            } else {
                scrollable_list(
                    ("playlist-entries-scrollbar", panel_id),
                    entries,
                    &self.browser.playlists.entries().scroll,
                )
                .into_any_element()
            };
            lists = lists.child(
                column()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list_viewport(entries)
                            .flex_none()
                            .flex_shrink_1()
                            .h(row_height * entries_len.max(1) as f32 + px(2.))
                            .max_h(gpui::relative(1.)),
                    )
                    .child(
                        div()
                            .id(("playlist-append", panel_id))
                            .flex_1()
                            .min_h(gpui::rems(1.))
                            .w_full()
                            .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                                window.prevent_default();
                                this.move_playlist_entries(drag, None, cx);
                            }))
                            .can_drop({
                                let owner = cx.weak_entity();
                                move |value, _, cx| {
                                    owner.upgrade().is_some_and(|owner| {
                                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                                            owner.read(cx).can_drop_playlist_entries(drag, None)
                                        })
                                    })
                                }
                            })
                            .drag_over::<PlaylistDrag>(|style, _, _, _| style.bg(rgb(HIGHLIGHT))),
                    ),
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
                }
            }))
            .can_drop({
                let owner = cx.weak_entity();
                move |value, _, cx| {
                    owner.upgrade().is_some_and(|owner| {
                        let app = owner.read(cx);
                        value.downcast_ref::<LibraryDrag>().is_some_and(|drag| {
                            app.browser.playlists.selected().is_some()
                                && app.can_resolve_library_drag(drag)
                        })
                    })
                }
            })
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)))
            .child(panel_header(
                self.tr("gl-playlists"),
                self.i18n.format(
                    "gl-playlist-count",
                    &[
                        ("total", (self.browser.playlists.summaries().total).into()),
                        (
                            "selected",
                            (self.browser.playlists.track_selection.len()).into(),
                        ),
                    ],
                ),
            ))
            .child(
                row()
                    .min_h(gpui::rems(2.))
                    .flex_wrap()
                    .flex_shrink_0()
                    .child(icon_button(
                        ("playlist-create", panel_id),
                        "",
                        self.tr("gl-new-playlist"),
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
                        self.tr("gl-import-m3u"),
                        cx,
                        |this, _, cx| this.choose_playlist_import(cx),
                    ))
                    .child(disabled_control(
                        icon_button(
                            ("playlist-export", panel_id),
                            "",
                            self.tr("gl-export-playlist"),
                            cx,
                            move |this, _, cx| {
                                if let Some(id) = playlist_id {
                                    this.choose_playlist_export(id, cx);
                                }
                            },
                        ),
                        playlist_id.is_none(),
                    ))
                    .when(self.browser.playlists.track_selection.len() > 0, |tools| {
                        let disabled = !self.browser.playlists.track_selection.ready();
                        tools.child(disabled_control(
                            icon_button(
                                ("playlist-remove-selected", panel_id),
                                "",
                                self.tr("gl-remove-entries"),
                                cx,
                                |this, _, cx| this.remove_playlist_selection(cx),
                            ),
                            disabled,
                        ))
                    })
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
        let owner = cx.weak_entity();
        window.defer(cx, move |_, cx| {
            let _ = owner.update(cx, |this, cx| {
                this.sync_metadata_query();
                this.demand_metadata(cx);
            });
        });
        self.sync_metadata_inputs(cx);
        self.prepare_artwork(cx);
        let track = self.metadata.track();
        let id = track.map(|track| track.id);
        let favorite = track.is_some_and(|track| track.favorite);
        let editable = self.metadata.editable();
        let editor_identity = self.metadata.editor_identity();
        let protected = self.metadata_protected(cx);
        let can_save = self.metadata.can_save(MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
            decoder_hint: self.metadata_decoder_hint,
        });
        let identity = self.metadata.identity();
        let follow_paused = self.metadata.selected().is_none()
            && protected
            && identity != self.state.current_track().map(|track| track.id);
        let placeholder: SharedString = self.tr("gl-not-loaded");
        let presence_status = match self.metadata.availability() {
            MetadataAvailability::Unknown if identity.is_none() => self.i18n.text("gl-no-track"),
            MetadataAvailability::Unknown => self.i18n.text("gl-details-not-loaded"),
            MetadataAvailability::Present(_) => self.i18n.text("gl-ready"),
            MetadataAvailability::Unavailable(_) => self.i18n.text("gl-track-unavailable"),
        };
        let status: SharedString = match self.metadata.read_state() {
            MetadataReadState::Idle => presence_status.to_owned().into(),
            MetadataReadState::Pending(_) => self
                .i18n
                .format(
                    "gl-details-loading-status",
                    &[("status", (presence_status).into())],
                )
                .into(),
            MetadataReadState::Failed(error) => self
                .i18n
                .format(
                    "gl-details-failed-status",
                    &[
                        ("status", (presence_status).into()),
                        ("error", (error.as_str()).into()),
                    ],
                )
                .into(),
            MetadataReadState::WrongTrack => self
                .i18n
                .format(
                    "gl-details-failed-status",
                    &[
                        ("status", presence_status.into()),
                        ("error", self.i18n.text("gl-metadata-wrong-track").into()),
                    ],
                )
                .into(),
        };
        let status: SharedString = if protected {
            self.i18n
                .format(
                    "gl-details-protected-status",
                    &[
                        ("status", (status.as_ref()).into()),
                        (
                            "draft",
                            (self.i18n.text(if self.metadata.saving() {
                                "gl-metadata-saving"
                            } else {
                                "gl-unsaved"
                            }))
                            .into(),
                        ),
                        ("follow", (usize::from(follow_paused)).into()),
                    ],
                )
                .into()
        } else {
            status
        };
        let heading = self.metadata.selected().or(id).map_or_else(
            || self.i18n.text("gl-track-details").to_owned(),
            |id| {
                self.i18n
                    .format("gl-track-id", &[("id", (id.to_string()).into())])
            },
        );
        let file: [[(SharedString, SharedString); 2]; 1] = [[
            (
                self.tr("gl-file-path"),
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        if track.missing {
                            self.tr("gl-missing")
                        } else {
                            track.path.to_string_lossy().into_owned().into()
                        }
                    },
                ),
            ),
            (
                self.tr("gl-availability"),
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        if track.missing {
                            self.tr("gl-missing")
                        } else {
                            self.tr("gl-available")
                        }
                    },
                ),
            ),
        ]];
        let metadata: [[(SharedString, SharedString); 2]; 3] = [
            [
                (
                    self.tr("gl-codec"),
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
                    self.tr("gl-sample-rate"),
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
                    self.tr("gl-channels"),
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
                    self.tr("gl-duration"),
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
                    self.tr("gl-bitrate"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track.bitrate_bps.map_or("—".into(), |value| {
                                format!(
                                    "{}{:.1} kbps",
                                    if track.bitrate_exact { "" } else { "≈" },
                                    value as f64 / 1000.0
                                )
                                .into()
                            })
                        },
                    ),
                ),
                (
                    self.tr("gl-bits-sample"),
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
        let disc: (SharedString, SharedString) = (
            self.tr("gl-disc"),
            track.map_or_else(
                || placeholder.clone(),
                |track| {
                    track
                        .disc_number
                        .map_or("—".into(), |value| value.to_string().into())
                },
            ),
        );
        let tags: [[(SharedString, SharedString); 2]; 2] = [
            [
                (
                    self.tr("gl-track"),
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
                    self.tr("gl-release"),
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
                    self.tr("gl-play-count"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| track.play_count.to_string().into(),
                    ),
                ),
                (
                    self.tr("gl-last-played"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| self.last_played_text(track.id, track.last_played).into(),
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
                .map(|fields| &fields[index].0)
                .chain((index == 1).then_some(&disc.0))
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
                        .shape_line(label.clone(), window.rem_size() * 0.75, &[run], None)
                        .width()
                        .ceil()
                })
                .fold(px(0.), gpui::Pixels::max)
        });
        let path_tooltip: SharedString = track
            .filter(|track| track.missing)
            .map(|track| track.path.to_string_lossy().into_owned().into())
            .unwrap_or_else(|| file[0][0].1.clone());
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
                        column()
                            .gap_1()
                            .child(
                                row()
                                    .justify_between()
                                    .items_center()
                                    .child(caption(heading).text_lg().text_color(rgb(TEXT)))
                                    .child(
                                        row()
                                            .gap_2()
                                            .child(disabled_control(
                                                icon_button(
                                                    ("metadata-favorite", panel_id),
                                                    if favorite { "󰓎" } else { "󰓐" },
                                                    if favorite {
                                                        self.tr("gl-unfavorite")
                                                    } else {
                                                        self.tr("gl-favorite")
                                                    },
                                                    cx,
                                                    move |this, _, cx| {
                                                        if this.metadata.editor_identity()
                                                            != editor_identity
                                                        {
                                                            return;
                                                        }
                                                        let Some((track_id, favorite)) =
                                                            this.metadata.track().map(|track| {
                                                                (track.id, track.favorite)
                                                            })
                                                        else {
                                                            return;
                                                        };
                                                        this.send(
                                                            Command::SetFavorite {
                                                                track_ids: vec![track_id],
                                                                favorite: !favorite,
                                                            },
                                                            cx,
                                                        );
                                                    },
                                                )
                                                .text_color(rgb(if favorite {
                                                    ACCENT
                                                } else {
                                                    TEXT
                                                })),
                                                !editable,
                                            ))
                                            .child(disabled_control(
                                                icon_button(
                                                    ("metadata-save", panel_id),
                                                    "󰆓",
                                                    self.tr("gl-save-metadata"),
                                                    cx,
                                                    move |this, _, cx| {
                                                        if this.metadata.editor_identity()
                                                            == editor_identity
                                                        {
                                                            this.save_metadata(cx);
                                                        }
                                                    },
                                                ),
                                                !can_save,
                                            ))
                                            .child(icon_button(
                                                ("metadata-retry", panel_id),
                                                "󰑐",
                                                self.tr("gl-reload-playback"),
                                                cx,
                                                move |this, _, cx| {
                                                    if this.metadata.editor_identity()
                                                        == editor_identity
                                                    {
                                                        this.reload_playback_metadata(cx);
                                                    }
                                                },
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("metadata-status", panel_id))
                                    .min_w_0()
                                    .h(gpui::rems(1.25))
                                    .child(
                                        caption(status.clone())
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .truncate(),
                                    )
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| super::ButtonTooltip {
                                            text: status.clone(),
                                        })
                                        .into()
                                    }),
                            ),
                    )
                    .child(metadata_group(
                        self.tr("gl-file"),
                        metadata_row(
                            div().flex_1().min_w_0().child(
                                metadata_item(
                                    file[0][0].0.clone(),
                                    file[0][0].1.clone(),
                                    label_widths[0],
                                )
                                .id(("metadata-path", panel_id))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| super::ButtonTooltip {
                                        text: path_tooltip.clone(),
                                    })
                                    .into()
                                }),
                            ),
                            metadata_item(
                                file[0][1].0.clone(),
                                file[0][1].1.clone(),
                                label_widths[1],
                            ),
                        ),
                    ))
                    .child(metadata_group(
                        self.tr("gl-metadata"),
                        metadata_rows(metadata, label_widths),
                    ))
                    .child(metadata_group(
                        self.tr("gl-decoder-hint"),
                        column()
                            .gap_2()
                            .child(
                                row().flex_wrap().children(
                                    [
                                        (
                                            "metadata-decoder-auto",
                                            self.tr("gl-auto"),
                                            DecoderHint::Auto,
                                        ),
                                        (
                                            "metadata-decoder-native",
                                            self.tr("gl-native"),
                                            DecoderHint::Native,
                                        ),
                                        (
                                            "metadata-decoder-ffmpeg",
                                            SharedString::from("FFmpeg"),
                                            DecoderHint::Ffmpeg,
                                        ),
                                    ]
                                    .into_iter()
                                    .map(
                                        |(key, label, decoder_hint)| {
                                            disabled_control(
                                                active_control(
                                                    button(
                                                        (key, panel_id),
                                                        label,
                                                        cx,
                                                        move |this, _, cx| {
                                                            if this.metadata.editor_identity()
                                                                == editor_identity
                                                                && this.metadata.editable()
                                                            {
                                                                this.metadata_decoder_hint =
                                                                    decoder_hint;
                                                                cx.notify();
                                                            }
                                                        },
                                                    ),
                                                    editable
                                                        && self.metadata_decoder_hint
                                                            == decoder_hint,
                                                ),
                                                !editable,
                                            )
                                        },
                                    ),
                                ),
                            )
                            .child(
                                caption(self.tr("gl-decoder-save-note"))
                                    .text_xs()
                                    .text_color(rgb(MUTED)),
                            )
                            .child(
                                caption(self.tr("gl-decoder-warning"))
                                    .text_xs()
                                    .text_color(rgb(MUTED)),
                            ),
                    ))
                    .child(metadata_group(
                        self.tr("gl-tags"),
                        column()
                            .gap_2()
                            .child(metadata_row(
                                self.panel_field(Field::Title, self.tr("gl-title")),
                                self.panel_field(Field::Artist, self.tr("gl-artist")),
                            ))
                            .child(metadata_row(
                                self.panel_field(Field::Album, self.tr("gl-album")),
                                metadata_item(disc.0, disc.1, label_widths[1]),
                            ))
                            .child(metadata_rows(tags, label_widths).gap_2())
                            .when(track.is_some(), |tags| {
                                tags.child(match self.artwork_view.image(cx) {
                                    Some(image) => artwork_preview(
                                        self.tr("shell-panel-album-artwork"),
                                        image.size_full(),
                                        self.artwork_view.aspect_ratio(cx),
                                    ),
                                    None => artwork_preview(
                                        self.tr("shell-panel-album-artwork"),
                                        self.default_album
                                            .clone()
                                            .cached(gpui::StyleRefinement::default().size_full()),
                                        Some(1.0),
                                    ),
                                })
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
        label: impl Into<SharedString>,
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
        .px_2()
        .py_1()
        .rounded_sm()
    }

    pub(super) fn history_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let history_len = self.browser.history.len();
        let playback = (
            self.state.current_track().map(|track| track.id),
            self.state.playback.status,
            self.state.playback.last_heard_at.map(|time| {
                if self.state.playback.status == PlaybackStatus::Paused {
                    time
                } else {
                    0
                }
            }),
        );
        let source = &self.state.library.history;
        let rem_size = window.rem_size();
        let widths = if let Some(cache) = &self.history_column_widths
            && std::sync::Arc::ptr_eq(&cache.source, source)
            && cache.font == self.ui_font
            && cache.rem_size == rem_size
            && cache.playback == playback
        {
            cache.widths
        } else {
            let measure = |text: SharedString, font: gpui::Font| {
                let run = gpui::TextRun {
                    len: text.len(),
                    font,
                    color: rgb(MUTED).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window
                    .text_system()
                    .shape_line(text, rem_size * 0.75, &[run], None)
                    .width()
                    .ceil()
            };
            let text_width = |text: SharedString| measure(text, self.ui_font.clone());
            // Keep space for either sort direction so sorting does not move columns.
            let arrow = measure("󰁅".into(), gpui::font(super::fonts::SYMBOLS_FAMILY)).max(measure(
                "󰁝".into(),
                gpui::font(super::fonts::SYMBOLS_FAMILY),
            )) + rem_size * 0.25;
            let mut widths = [
                text_width("#".into()),
                text_width(self.tr("gl-play-count")) + arrow,
                text_width(self.tr("gl-last-played")) + arrow,
            ];
            for (index, item) in source.iter().enumerate() {
                widths[0] = widths[0].max(text_width((index + 1).to_string().into()));
                widths[1] = widths[1].max(text_width(item.play_count.to_string().into()));
                let last_played = self.last_played_text(item.track_id, Some(item.played_at));
                widths[2] = widths[2].max(text_width(last_played.into()));
            }
            // Match px_2 on both header controls and data cells.
            let widths = widths.map(|width| width + rem_size);
            self.history_column_widths = Some(HistoryColumnWidths {
                source: std::sync::Arc::clone(source),
                font: self.ui_font.clone(),
                rem_size,
                playback,
                widths,
            });
            widths
        };
        let header = row()
            .items_center()
            .gap_0()
            .px_2()
            .py_1()
            .h(gpui::rems(2.625))
            .flex_shrink_0()
            .min_w_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                caption("#")
                    .w(widths[0])
                    .flex_shrink_0()
                    .px_2()
                    .min_w_0()
                    .truncate(),
            )
            .child(self.history_sort_button(
                panel_id,
                self.tr("gl-track"),
                HistorySort::Title,
                None,
                cx,
            ))
            .child(self.history_sort_button(
                panel_id,
                self.tr("gl-play-count"),
                HistorySort::Plays,
                Some(widths[1]),
                cx,
            ))
            .child(self.history_sort_button(
                panel_id,
                self.tr("gl-last-played"),
                HistorySort::Recent,
                Some(widths[2]),
                cx,
            ))
            .child(div().w(px(12.)).flex_shrink_0());
        let scroll = self.scroll_views.list(("history", panel_id));
        panel_surface(("history-panel", panel_id))
            .child(panel_header(
                self.tr("gl-recent-tracks"),
                self.i18n
                    .format("gl-track-count", &[("count", (history_len).into())]),
            ))
            .child(list_viewport(
                column().size_full().child(header).child(scrollable_list(
                    ("history-scrollbar", panel_id),
                    uniform_list(
                        ("history-rows", panel_id),
                        history_len,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .filter_map(|index| {
                                    let item = this.browser.history.get(index)?;
                                    let id = item.track_id;
                                    let favorite = item.favorite;
                                    let title = if item.title.is_empty() {
                                        this.i18n.text("gl-untitled").to_owned()
                                    } else {
                                        item.title.clone()
                                    };
                                    let last_played =
                                        this.last_played_text(id, Some(item.played_at));
                                    Some(
                                        list_row(
                                            ("history-entry", id as u64),
                                            this.browser.library.selected().contains(&id),
                                        )
                                        .gap_0()
                                        .group("history-track-row")
                                        .relative()
                                        .child(
                                            caption((index + 1).to_string())
                                                .w(widths[0])
                                                .flex_shrink_0()
                                                .px_2()
                                                .min_w_0()
                                                .truncate(),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .px_2()
                                                .text_sm()
                                                .truncate()
                                                .child(title),
                                        )
                                        .child(
                                            caption(item.play_count.to_string())
                                                .w(widths[1])
                                                .flex_shrink_0()
                                                .px_2()
                                                .min_w_0()
                                                .truncate(),
                                        )
                                        .child(
                                            caption(last_played.to_owned())
                                                .w(widths[2])
                                                .flex_shrink_0()
                                                .px_2()
                                                .min_w_0()
                                                .truncate(),
                                        )
                                        .child(
                                            hover_actions(
                                                "history-track-row",
                                                this.browser.library.selected().contains(&id),
                                            )
                                            .child(
                                                this.track_favorite_toggle(
                                                    ("history-entry-favorite", id as u64),
                                                    "history-track-row",
                                                    id,
                                                    favorite,
                                                    cx,
                                                ),
                                            ),
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
                    &scroll,
                )),
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod playlist_drag_tests {
    use super::PlaylistDrag;

    #[test]
    fn playlist_drop_rejects_empty_groups_and_selected_targets() {
        let drag = PlaylistDrag {
            i18n: std::sync::Arc::new(crate::i18n::Localizer::new("en-US")),
            playlist_id: 8,
            source_id: 40,
            entry_ids: std::cell::OnceCell::from(vec![7, 40, 2]),
            revision: 19,
            stamp: Default::default(),
            selection_generation: 4,
        };
        for target in [7, 40, 2] {
            assert!(drag.command(Some(target)).is_none());
        }
        let empty = PlaylistDrag {
            entry_ids: Default::default(),
            ..drag
        };
        assert!(empty.command(None).is_none());
    }
}
