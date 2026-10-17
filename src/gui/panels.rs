use super::{
    Dragging, Field, GuiApp, HistorySort, Measured,
    components::{
        active_control, caption, column, compact_icon_button, disabled_control, form_field,
        hover_actions, icon_button, list_panel_header, list_row, list_viewport, panel_surface,
        quiet_icon_button, row, scrollable_list, sortable_header,
    },
    fonts::tabular_numbers,
    format_time,
};
use crate::model::{Command, PlaybackStatus, RepeatMode};
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

impl GuiApp {
    pub(super) fn last_played_text(&self, track_id: i64, stored_at: Option<i64>) -> String {
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
        let theme = crate::gui::theme::palette(cx);
        let track = self.state.current_track();
        let title = track
            .map(|track| track.title.clone())
            .unwrap_or_else(|| self.i18n.text("gl-choose-track").into());
        let artist = track.map(|track| track.artist.clone()).unwrap_or_default();
        let album = track.map(|track| track.album.clone()).unwrap_or_default();
        let has_artist = !artist.trim().is_empty();
        let has_album = !album.trim().is_empty();
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
                theme,
                quiet_icon_button(
                    theme,
                    icon_button(
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
                    ),
                )
                .rounded_full()
                .text_lg(),
                self.state.playback.shuffle,
            ))
            .child(
                quiet_icon_button(
                    theme,
                    icon_button(
                        ("previous", panel_id),
                        "󰒮",
                        self.tr("gl-previous-track"),
                        cx,
                        |this, _, cx| this.send(Command::Previous, cx),
                    ),
                )
                .rounded_full()
                .text_lg(),
            )
            .child(
                quiet_icon_button(
                    theme,
                    icon_button(
                        ("toggle", panel_id),
                        if self.state.playback.status == PlaybackStatus::Playing {
                            "󰏤"
                        } else {
                            ""
                        },
                        if self.state.playback.status == PlaybackStatus::Playing {
                            self.tr("gl-pause")
                        } else {
                            self.tr("gl-play")
                        },
                        cx,
                        |this, _, cx| this.send(Command::Toggle, cx),
                    ),
                )
                .bg(rgb(theme.control))
                .text_color(rgb(theme.text))
                .size(gpui::rems(2.5))
                .rounded_full()
                .text_xl(),
            )
            .child(
                quiet_icon_button(
                    theme,
                    icon_button(
                        ("next", panel_id),
                        "󰒭",
                        self.tr("gl-next-track"),
                        cx,
                        |this, _, cx| this.send(Command::Next, cx),
                    ),
                )
                .rounded_full()
                .text_lg(),
            )
            .child(active_control(
                theme,
                quiet_icon_button(
                    theme,
                    icon_button(
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
                    ),
                )
                .rounded_full()
                .text_lg(),
                self.state.playback.repeat != RepeatMode::Off,
            ));
        let volume_control = row()
            .gap_1()
            .w(gpui::rems(9.))
            .max_w(gpui::relative(1.))
            .flex_shrink_0()
            .child(
                caption(theme, format!("{:.0}%", self.state.playback.volume * 100.))
                    .font_features(tabular_numbers())
                    .whitespace_nowrap()
                    .flex_shrink_0(),
            )
            .child(
                div()
                    .id(("volume-label", panel_id))
                    .flex_shrink_0()
                    .text_lg()
                    .text_color(rgb(theme.muted))
                    .font_family(super::fonts::SYMBOLS_FAMILY)
                    .child("󰕾")
                    .tooltip({
                        let text = self.tr("gl-volume");
                        move |_, cx| {
                            cx.new(|_| crate::gui::components::ButtonTooltip { text: text.clone() })
                                .into()
                        }
                    }),
            )
            .child(div().flex_1().min_w_0().child(volume));
        let scroll = self.scroll_views.content(("transport", panel_id));
        column()
            .id(("transport-panel", panel_id))
            .size_full()
            .min_w_0()
            .min_h_0()
            .gap_2()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .child(
                row()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        row()
                            .gap_0()
                            .flex_shrink_0()
                            .child(disabled_control(
                                theme,
                                quiet_icon_button(
                                    theme,
                                    icon_button(
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
                                    ),
                                )
                                .text_color(rgb(theme.muted).alpha(0.4))
                                .when(
                                    self.state.queue.current_id.is_some(),
                                    |control| {
                                        control
                                            .group_hover("control", |style| {
                                                style.text_color(rgb(theme.text))
                                            })
                                            .focus(|style| style.text_color(rgb(theme.text)))
                                    },
                                ),
                                self.state.queue.current_id.is_none(),
                            ))
                            .child(caption(theme, status)),
                    ),
            )
            .child(
                row()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .child(
                        row()
                            .flex_1()
                            .min_w_0()
                            .when(has_artist, |view| {
                                view.child(
                                    caption(theme, artist)
                                        .text_sm()
                                        .text_color(rgb(theme.text))
                                        .min_w_0()
                                        .truncate(),
                                )
                            })
                            .when(has_artist && has_album, |view| {
                                view.child(caption(theme, "·").flex_shrink_0())
                            })
                            .when(has_album, |view| {
                                view.child(
                                    caption(theme, album)
                                        .text_color(rgb(theme.secondary))
                                        .min_w_0()
                                        .truncate(),
                                )
                            }),
                    )
                    .child(
                        caption(
                            theme,
                            format!(
                                "{} / {}",
                                format_time(preview_position),
                                duration.map_or("—".into(), format_time)
                            ),
                        )
                        .font_features(tabular_numbers())
                        .whitespace_nowrap()
                        .flex_shrink_0(),
                    ),
            )
            .child(seek)
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
    pub(super) fn panel_field(
        &self,
        theme: &'static crate::theme::Palette,
        field: Field,
        label: impl Into<SharedString>,
    ) -> Div {
        form_field(theme, label, self.input(field))
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

    pub(super) fn track_favorite_toggle(
        &self,
        id: impl Into<gpui::ElementId>,
        group: &'static str,
        track_id: i64,
        favorite: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let theme = crate::gui::theme::palette(cx);
        compact_icon_button(
            theme,
            icon_button(
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
            ),
        )
        .when(favorite, |view| view.text_color(rgb(theme.accent)))
        .opacity(0.)
        .group_hover(group, |style| style.opacity(1.))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
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
        let theme = crate::gui::theme::palette(cx);
        use super::list_interaction::{ContentDrag, ContentMenuRequest, TrackSource};

        let history_len = self.browser.history.len();
        let history_total = self.browser.history.total();
        self.history_search.update(cx, |search, cx| {
            search.set_counts(history_len, history_total, cx)
        });
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
                    color: rgb(theme.muted).into(),
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
            let numeric_width = |text: SharedString| {
                let mut font = self.ui_font.clone();
                font.features = tabular_numbers();
                measure(text, font)
            };
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
                widths[0] = widths[0].max(numeric_width((index + 1).to_string().into()));
                widths[1] = widths[1].max(numeric_width(item.play_count.to_string().into()));
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
            .border_color(rgb(theme.border))
            .child(
                caption(theme, "#")
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
            .child(
                row()
                    .items_end()
                    .child(
                        list_panel_header(
                            theme,
                            self.tr("gl-recent-tracks"),
                            self.i18n
                                .format("gl-track-count", &[("count", history_total.into())]),
                        )
                        .flex_1()
                        .min_w(gpui::rems(3.)),
                    )
                    .child(self.history_search.clone()),
            )
            .when(history_len == 0, |panel| {
                panel.child(caption(theme, self.tr(if history_total == 0 {
                    "search-empty"
                } else {
                    "search-no-matches"
                })).p_3())
            })
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
                                    let selected = this.browser.history.contains(id);
                                    let generation = this.browser.history.generation();
                                    let drag_owner = cx.weak_entity();
                                    let drag_i18n = this.i18n.clone();
                                    Some(
                                        list_row(
                                            theme,
                                            ("history-entry", id as u64),
                                            selected,
                                        )
                                        .gap_0()
                                        .group("history-track-row")
                                        .relative()
                                        .child(
                                            caption(theme, (index + 1).to_string())
                                                .font_features(tabular_numbers())
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
                                            caption(theme, item.play_count.to_string())
                                                .font_features(tabular_numbers())
                                                .w(widths[1])
                                                .flex_shrink_0()
                                                .px_2()
                                                .min_w_0()
                                                .truncate(),
                                        )
                                        .child(
                                            caption(theme, last_played.to_owned())
                                                .w(widths[2])
                                                .flex_shrink_0()
                                                .px_2()
                                                .min_w_0()
                                                .truncate(),
                                        )
                                        .child(
                                            hover_actions(
                                                theme,
                                                "history-track-row",
                                                selected,
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
                                        .on_mouse_down(
                                            gpui::MouseButton::Right,
                                            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                                cx.stop_propagation();
                                                this.focus_layout(window, cx);
                                                this.browser.focus_history();
                                                this.refresh(cx);
                                                let ids = this.browser.history.drag_ids(id, generation);
                                                if ids.is_empty() {
                                                    return;
                                                }
                                                if !this.browser.history.contains(id) {
                                                    this.select_history_track(id, false, false, cx);
                                                }
                                                this.open_content_menu(ContentMenuRequest {
                                                    source: TrackSource::Tracks(ids),
                                                    position: event.position,
                                                    play: Some(Command::Play { track_id: id }),
                                                }, cx);
                                            }),
                                        )
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, window, cx| {
                                                this.focus_layout(window, cx);
                                                this.browser.focus_history();
                                                let modifiers = event.modifiers();
                                                this.select_history_track(
                                                    id,
                                                    modifiers.shift,
                                                    modifiers.control,
                                                    cx,
                                                );
                                                if event.click_count() == 2 {
                                                    this.send(Command::Play { track_id: id }, cx);
                                                }
                                            },
                                        ))
                                        .on_drag(ContentDrag::pending(), move |drag, _, window, cx| {
                                            let ids = drag_owner
                                                .update(cx, |this, cx| {
                                                    this.refresh(cx);
                                                    let ids = this.browser.history.drag_ids(id, generation);
                                                    if !ids.is_empty() && !this.browser.history.contains(id) {
                                                        this.select_history_track(id, false, false, cx);
                                                    }
                                                    ids
                                                })
                                                .unwrap_or_default();
                                            let count = ids.len();
                                            drag.initialize(TrackSource::Tracks(ids));
                                            super::components::begin_list_drag(window, cx);
                                            let label = drag_i18n.format(
                                                "gl-track-count",
                                                &[("count", count.into())],
                                            ).into();
                                            cx.new(|_| super::components::ListDragPreview { label })
                                        }),
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
