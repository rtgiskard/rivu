use super::{
    Dragging, Field, GuiApp, HistorySort, ListFocus, Measured, UI_INSET, ViewRequestKind,
    components::{
        LIST_ROW_REMS, StatusKind, active_control, caption, column, compact_icon_button,
        danger_control, disabled_control, form_field, hover_actions, icon_button, list_row,
        list_row_height, list_viewport, panel_header, panel_surface, panel_toolbar,
        quiet_icon_button, row, row_text, scrollable_list, sortable_header, status_message,
        track_list_row,
    },
    fonts::tabular_numbers,
    format_time,
    library::LibraryDrag,
    paged,
};
use crate::{
    gui::queue::QueueDrag,
    model::{Command, PlaybackStatus, RepeatMode},
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

pub(super) struct PlaylistDrag {
    i18n: std::sync::Arc<crate::i18n::Localizer>,
    playlist_id: i64,
    source_id: i64,
    // GPUI keeps the original on_drag value; the constructor creates only its preview.
    entry_ids: std::cell::OnceCell<Vec<i64>>,
    track_ids: std::cell::OnceCell<Vec<i64>>,
    revision: u64,
    stamp: crate::model::QueryStamp,
    selection_generation: u64,
}

impl PlaylistDrag {
    pub(super) fn targets_source(&self, destination: super::DragDestination) -> bool {
        matches!(destination, super::DragDestination::Playlist(id) if id == self.playlist_id)
    }

    pub(super) fn can_transfer(&self, app: &GuiApp, destination: super::DragDestination) -> bool {
        if self.targets_source(destination) {
            return app.can_drop_playlist_entries(self, None);
        }
        app.state
            .library
            .playlist_entry_revisions
            .get(&self.playlist_id)
            == Some(&self.revision)
            && self.track_ids.get().is_some_and(|ids| !ids.is_empty())
    }

    pub(super) fn track_ids(&self) -> Vec<i64> {
        self.track_ids.get().unwrap().clone()
    }

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

    fn can_drop_playlist_entries(&self, drag: &PlaylistDrag, target: Option<i64>) -> bool {
        self.browser.playlists.selected() == Some(drag.playlist_id)
            && self.browser.playlists.entries().query.is_none()
            && self.browser.playlists.entries().playlist_id == Some(drag.playlist_id)
            && self.browser.playlists.selection_generation() == drag.selection_generation
            && self.playlist_occurrence_stamp(drag.playlist_id) == drag.stamp
            && self.playlist_revision(drag.playlist_id) == drag.revision
            && drag.accepts(target)
    }

    pub(super) fn move_playlist_entries(
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
        let theme = crate::gui::theme::palette(cx);
        let Some(action) = self.playlist_action.clone() else {
            return div().into_any_element();
        };
        if let PlaylistAction::Menu(target, position) = action {
            let id = target.id;
            let has_tracks = self.state.current_track().is_some();
            let rename = target.clone();
            let delete = target.clone();
            let menu = crate::gui::components::context_menu_container(theme, "playlist-row-menu")
                .map(|surface| self.menu_surface(surface))
                .max_w((window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.)))
                .max_h((window.viewport_size().height - px(UI_INSET * 2.)).max(px(0.)))
                .overflow_y_scroll()
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.dismiss_playlist_action(window, cx);
                    cx.stop_propagation();
                }))
                .child(crate::gui::components::menu_item(
                    "playlist-row-play",
                    self.tr("li-replace-queue"),
                    cx,
                    move |this, window, cx| {
                        this.refresh(cx);
                        if this.state.queue.total == 0 {
                            this.dismiss_playlist_action(window, cx);
                            this.send(Command::PlayPlaylist { playlist_id: id }, cx);
                        } else {
                            let name = match this.playlist_action.take() {
                                Some(PlaylistAction::Menu(target, _)) => target.name,
                                _ => return,
                            };
                            this.playlist_action =
                                Some(PlaylistAction::Play(PlaylistTarget { id, name }));
                            cx.notify();
                        }
                    },
                ))
                .child(crate::gui::components::menu_item(
                    "playlist-row-queue",
                    self.tr("li-queue"),
                    cx,
                    move |this, window, cx| {
                        this.dismiss_playlist_action(window, cx);
                        this.append_playlist_to_queue(id, cx);
                    },
                ))
                .child(super::components::menu_separator(theme))
                .child(
                    crate::gui::components::menu_item(
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
                .child(crate::gui::components::menu_item(
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
                .child(super::components::danger_menu_item(
                    theme,
                    crate::gui::components::menu_item(
                        "playlist-row-delete",
                        self.tr("gl-delete"),
                        cx,
                        move |this, _, cx| {
                            this.playlist_action = Some(PlaylistAction::Delete(delete.clone()));
                            cx.notify();
                        },
                    ),
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
            .map(|surface| self.dialog_surface(surface))
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
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .rounded_md()
                    .child(panel_header(theme, title, detail).flex_shrink_0())
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
                            .child(danger_control(
                                theme,
                                icon_button(
                                    "playlist-action-confirm",
                                    if deleting { "󰆴" } else { "" },
                                    if deleting {
                                        self.tr("gl-delete-playlist")
                                    } else {
                                        self.tr("gl-replace-playlist")
                                    },
                                    cx,
                                    move |this, window, cx| {
                                        this.dismiss_playlist_action(window, cx);
                                        if deleting {
                                            if this.browser.playlists.selected() == Some(target.id)
                                            {
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
                                ),
                            )),
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
        let theme = crate::gui::theme::palette(cx);
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
        } else if (kind == ViewRequestKind::Entries
            && self.browser.playlists.entries().query.is_some()
            && self
                .browser
                .playlists
                .selected_total()
                .is_some_and(|total| total > 0))
            || (kind == ViewRequestKind::Summaries
                && self.browser.playlists.summaries().query.is_some()
                && self.state.library.playlist_total > 0)
        {
            self.i18n.text("search-no-matches")
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
                    theme,
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
                    cx.new(|_| crate::gui::components::ButtonTooltip {
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
        let theme = crate::gui::theme::palette(cx);
        let owner = cx.weak_entity();
        window.defer(cx, move |_, cx| {
            let _ = owner.update(cx, |this, cx| this.request_playlist_selection(cx));
        });
        let matched = self.browser.playlists.summaries().rows.total();
        let total = self.state.library.playlist_total;
        self.playlist_search.update(cx, |search, cx| {
            if let Some(matched) = matched {
                search.set_counts(matched, total, cx);
            } else {
                search.clear_counts(cx);
            }
        });
        let matched = self.browser.playlists.entries().rows.total();
        let total = self.browser.playlists.selected_total();
        self.playlist_entries_search.update(cx, |search, cx| {
            if let (Some(matched), Some(total)) = (matched, total) {
                search.set_counts(matched, total, cx);
            } else {
                search.clear_counts(cx);
            }
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
            .flex_shrink_0()
            .min_w_0()
            .h(gpui::rems(2.))
            .overflow_hidden()
            .child(
                status_message(theme, selection_message.clone(), StatusKind::Info)
                    .id(("playlist-selection-status", panel_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::gui::components::ButtonTooltip {
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
            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
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
                            theme,
                            ("playlist", id as u64),
                            this.browser.playlists.selected() == Some(id),
                        )
                        .when(
                            this.layout_focus.is_focused(window)
                                && this.browser.focus() == Some(super::BrowserFocus::Playlists)
                                && this.browser.playlists.selected() == Some(id),
                            |row| row.border_color(rgb(theme.accent)),
                        )
                        .group("playlist-catalog-row")
                        .gap_0()
                        .child(row_text(
                            theme,
                            ("playlist-text", id as u64),
                            name.clone(),
                            this.i18n
                                .format("gl-track-count", &[("count", (p.entry_count).into())]),
                        ))
                        .child(
                            compact_icon_button(
                                theme,
                                icon_button(
                                    ("playlist-row-play", id as u64),
                                    "",
                                    this.tr("gl-play-playlist"),
                                    cx,
                                    move |this, window, cx| {
                                        if !playable {
                                            return;
                                        }
                                        this.refresh(cx);
                                        if this.state.queue.total == 0 {
                                            this.send(
                                                Command::PlayPlaylist { playlist_id: id },
                                                cx,
                                            );
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
                                ),
                            )
                            .opacity(0.)
                            .group_hover("playlist-catalog-row", move |style| {
                                style.opacity(if playable { 1. } else { 0.4 })
                            })
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            }),
                        )
                        .child(
                            compact_icon_button(
                                theme,
                                icon_button(
                                    ("playlist-row-menu", id as u64),
                                    "󰇙",
                                    this.tr("gl-playlist-actions"),
                                    cx,
                                    move |this, window, cx| {
                                        this.focus_layout(window, cx);
                                        this.content_menu = None;
                                        this.panel_menu = None;
                                        this.playlist_action = Some(PlaylistAction::Menu(
                                            PlaylistTarget {
                                                id,
                                                name: menu_name.to_string(),
                                            },
                                            window.mouse_position(),
                                        ));
                                        cx.notify();
                                    },
                                ),
                            )
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
                                this.content_menu = None;
                                this.panel_menu = None;
                                this.browser.focus_playlists();
                                this.select_playlist_container(
                                    id,
                                    context_name.to_string(),
                                    index,
                                    cx,
                                );
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
                        .on_drop(cx.listener(move |this, drag: &LibraryDrag, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.resolve_library_drag(
                                drag.nodes(),
                                super::DragDestination::Playlist(id),
                                cx,
                            );
                        }))
                        .on_drop(
                            cx.listener(move |this, drag: &super::ContentDrag, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.drop_content(drag, super::DragDestination::Playlist(id), cx);
                            }),
                        )
                        .on_drop(cx.listener(move |this, drag: &QueueDrag, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.transfer_queue_drag(
                                drag,
                                super::DragDestination::Playlist(id),
                                cx,
                            );
                        }))
                        .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.transfer_playlist_drag(
                                drag,
                                super::DragDestination::Playlist(id),
                                cx,
                            );
                        }))
                        .can_drop({
                            let owner = cx.weak_entity();
                            move |value, _, cx| {
                                owner.upgrade().is_some_and(|owner| {
                                    owner.read(cx).can_transfer_content(
                                        value,
                                        super::DragDestination::Playlist(id),
                                    )
                                })
                            }
                        })
                        .drag_over::<LibraryDrag>(|style, _, _, _| style.bg(rgb(theme.highlight)))
                        .drag_over::<super::ContentDrag>(|style, _, _, _| {
                            style.bg(rgb(theme.highlight))
                        })
                        .drag_over::<QueueDrag>(|style, _, _, _| style.bg(rgb(theme.highlight)))
                        .drag_over::<PlaylistDrag>(|style, _, _, _| style.bg(rgb(theme.highlight)))
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
            lists = lists.child(
                panel_toolbar()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, _| {
                            this.browser.focus_list(ListFocus::PlaylistTracks)
                        }),
                    )
                    .child(div().flex_1().min_w_0().child(caption(
                        theme,
                        total.map_or_else(
                            || "—".to_owned(),
                            |count| {
                                self.i18n
                                    .format("gl-track-count", &[("count", count.into())])
                            },
                        ),
                    )))
                    .child(self.playlist_entries_search.clone()),
            );
            self.scroll_views.register_list(
                ("playlist-entries", panel_id),
                &self.browser.playlists.entries().scroll,
            );
            let entries_len = self.browser.playlists.entries().rows.len();
            let entries = uniform_list(
                ("playlist-entries", panel_id),
                entries_len,
                cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
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
                                track_ids: Default::default(),
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
                                theme,
                                ("playlist-entry", id as u64),
                                ("playlist-entry-text", id as u64),
                                (title, detail),
                                index,
                                this.state
                                    .current_track()
                                    .is_some_and(|track| track.id == track_id),
                                this.browser.playlists.track_selection.contains(id),
                            )
                            .when(
                                this.layout_focus.is_focused(window)
                                    && this.browser.focus()
                                        == Some(super::BrowserFocus::List(
                                            ListFocus::PlaylistTracks,
                                        ))
                                    && this.browser.playlists.selected_entry() == Some(id),
                                |row| row.border_color(rgb(theme.accent)),
                            )
                            .group("playlist-track-row")
                            .line_height(gpui::rems(1.125))
                            .relative()
                            .gap_0()
                            .child(
                                hover_actions(
                                    theme,
                                    "playlist-track-row",
                                    this.browser.playlists.track_selection.contains(id),
                                )
                                .child(
                                    compact_icon_button(
                                        theme,
                                        icon_button(
                                            ("playlist-entry-play", id as u64),
                                            "",
                                            this.tr("gl-play-track"),
                                            cx,
                                            move |this, _, cx| {
                                                this.send(Command::Play { track_id }, cx)
                                            },
                                        ),
                                    )
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
                                    compact_icon_button(
                                        theme,
                                        icon_button(
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
                                        ),
                                    )
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
                            .on_mouse_down(
                                gpui::MouseButton::Right,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        this.focus_layout(window, cx);
                                        this.browser.focus_list(ListFocus::PlaylistTracks);
                                        if this.browser.playlists.track_selection.contains(id)
                                            && !this.browser.playlists.track_selection.ready()
                                        {
                                            cx.stop_propagation();
                                            return;
                                        }
                                        if !this.browser.playlists.track_selection.contains(id) {
                                            this.select_playlist_entry(id, index, false, false, cx);
                                        }
                                        let rows = this.browser.playlists.track_selection.ordered();
                                        let track_ids = if rows.iter().any(|row| row.id == id) {
                                            rows.iter().map(|row| row.track_id).collect()
                                        } else {
                                            vec![track_id]
                                        };
                                        let entry_ids = if rows.iter().any(|row| row.id == id) {
                                            rows.iter().map(|row| row.id).collect()
                                        } else {
                                            vec![id]
                                        };
                                        this.open_object_menu(
                                            super::ContentMenuRequest {
                                                source: super::TrackSource::Tracks(track_ids),
                                                position: event.position,
                                                play: Some(Command::Play { track_id }),
                                            },
                                            super::list_interaction::ContentGuard::Playlist(
                                                playlist_id,
                                                revision,
                                            ),
                                            Some(Command::RemovePlaylistEntries {
                                                playlist_id,
                                                entry_ids,
                                                expected_revision: revision,
                                            }),
                                            None,
                                            None,
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    },
                                ),
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
                                        modifiers.control,
                                        cx,
                                    );
                                    if event.click_count() == 2 {
                                        this.send(Command::Play { track_id }, cx);
                                    }
                                    cx.notify();
                                },
                            ))
                            .when(
                                !selection_unavailable
                                    && this.browser.playlists.entries().query.is_none(),
                                |row| {
                                    row.on_drag(drag, move |drag, _, window, cx| {
                                        super::components::begin_list_drag(window, cx);
                                        drag.entry_ids.get_or_init(|| {
                                            drag_owner
                                                .update(cx, |this, cx| {
                                                    this.refresh(cx);
                                                    if this.playlist_revision(drag.playlist_id)
                                                        != drag.revision
                                                        || this
                                                            .browser
                                                            .playlists
                                                            .selection_generation()
                                                            != drag.selection_generation
                                                        || this
                                                            .browser
                                                            .playlists
                                                            .entries()
                                                            .query
                                                            .is_some()
                                                        || !this
                                                            .browser
                                                            .playlists
                                                            .track_selection
                                                            .ready()
                                                    {
                                                        return Vec::new();
                                                    }
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
                                                    let tracks = if this
                                                        .browser
                                                        .playlists
                                                        .track_selection
                                                        .contains(drag.source_id)
                                                    {
                                                        this.browser
                                                            .playlists
                                                            .track_selection
                                                            .ordered()
                                                            .iter()
                                                            .map(|row| row.track_id)
                                                            .collect()
                                                    } else {
                                                        vec![track_id]
                                                    };
                                                    let _ = drag.track_ids.set(tracks);
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
                                },
                            )
                            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                if drag.playlist_id == playlist_id {
                                    this.move_playlist_entries(drag, Some(id), cx);
                                } else {
                                    this.transfer_playlist_drag(
                                        drag,
                                        super::DragDestination::Playlist(playlist_id),
                                        cx,
                                    );
                                }
                            }))
                            .can_drop({
                                let owner = cx.weak_entity();
                                move |value, _, cx| {
                                    owner.upgrade().is_some_and(|owner| {
                                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                                            let app = owner.read(cx);
                                            if drag.playlist_id == playlist_id {
                                                app.can_drop_playlist_entries(drag, Some(id))
                                            } else {
                                                app.can_transfer_content(
                                                    drag,
                                                    super::DragDestination::Playlist(playlist_id),
                                                )
                                            }
                                        })
                                    })
                                }
                            })
                            .drag_over::<PlaylistDrag>(|style, _, _, _| {
                                style.bg(rgb(theme.highlight))
                            })
                            .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .with_decoration(paged::viewport_demand(
                cx.processor(|this, range, _, cx| this.ensure_entry_range(range, cx)),
            ))
            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                this.transfer_playlist_drag(
                    drag,
                    super::DragDestination::Playlist(playlist_id),
                    cx,
                );
            }))
            .can_drop({
                let owner = cx.weak_entity();
                move |value, _, cx| {
                    owner.upgrade().is_some_and(|owner| {
                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                            owner.read(cx).can_transfer_content(
                                drag,
                                super::DragDestination::Playlist(playlist_id),
                            )
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
                            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                                window.prevent_default();
                                this.transfer_playlist_drag(
                                    drag,
                                    super::DragDestination::Playlist(playlist_id),
                                    cx,
                                );
                            }))
                            .can_drop({
                                let owner = cx.weak_entity();
                                move |value, _, cx| {
                                    owner.upgrade().is_some_and(|owner| {
                                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                                            owner.read(cx).can_transfer_content(
                                                drag,
                                                super::DragDestination::Playlist(playlist_id),
                                            )
                                        })
                                    })
                                }
                            })
                            .drag_over::<PlaylistDrag>(|style, _, _, _| {
                                style.bg(rgb(theme.highlight))
                            }),
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
                        app.browser.playlists.selected().is_some_and(|id| {
                            app.can_transfer_content(value, super::DragDestination::Playlist(id))
                        })
                    })
                }
            })
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .on_drop(cx.listener(|this, drag: &super::ContentDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists.selected() {
                    this.drop_content(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &QueueDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists.selected() {
                    this.transfer_queue_drag(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists.selected() {
                    this.transfer_playlist_drag(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .drag_over::<super::ContentDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .drag_over::<PlaylistDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .child(
                row()
                    .min_h(gpui::rems(2.))
                    .flex_wrap()
                    .flex_shrink_0()
                    .items_end()
                    .child(
                        panel_header(
                            theme,
                            self.tr("gl-playlists"),
                            self.i18n.format(
                                "gl-playlist-count",
                                &[
                                    ("total", self.browser.playlists.summaries().total.into()),
                                    (
                                        "selected",
                                        self.browser.playlists.track_selection.len().into(),
                                    ),
                                ],
                            ),
                        )
                        .flex_1()
                        .min_w_0(),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, _| this.browser.focus_playlists()),
                    )
                    .child(self.playlist_search.clone())
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
                        theme,
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
                    )),
            )
            .when(selection_unavailable, |panel| panel.child(selection_status))
            .child(lists)
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
                        panel_header(
                            theme,
                            self.tr("gl-recent-tracks"),
                            self.i18n
                                .format("gl-track-count", &[("count", history_total.into())]),
                        )
                        .flex_1(),
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
            track_ids: std::cell::OnceCell::from(vec![9, 9, 3]),
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
