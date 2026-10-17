//! Playlist catalog, occurrence rows, drag and management surfaces.
pub(super) mod editor;
pub(super) mod selection;

use super::{
    GuiApp, ListFocus, UI_INSET, ViewRequestKind,
    components::{
        LIST_ROW_REMS, StatusKind, caption, column, compact_icon_button, context_menu_container,
        danger_control, disabled_control, hover_actions, icon_button, list_panel_header, list_row,
        list_row_height, list_viewport, panel_header, panel_surface, panel_toolbar, row, row_text,
        scrollable_list, status_message, track_list_row,
    },
    library::LibraryDrag,
    paged,
};
use crate::{gui::queue::QueueDrag, model::Command};
use gpui::{
    AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb, uniform_list,
};

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
    Delete(PlaylistTarget),
}

impl GuiApp {
    fn can_drop_playlist_entries(&self, drag: &PlaylistDrag, target: Option<i64>) -> bool {
        self.browser.playlists().selected() == Some(drag.playlist_id)
            && self.browser.playlists().entries().query.is_none()
            && self.browser.playlists().entries().playlist_id == Some(drag.playlist_id)
            && self.browser.playlists().selection_generation() == drag.selection_generation
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
            let menu = context_menu_container(theme, "playlist-row-menu", window)
                .map(|surface| self.menu_surface(surface))
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.dismiss_playlist_action(window, cx);
                    cx.stop_propagation();
                }))
                .child(crate::gui::components::menu_item(
                    "playlist-row-play",
                    self.tr("li-replace-queue"),
                    cx,
                    move |this, window, cx| {
                        let name = match this.playlist_action.take() {
                            Some(PlaylistAction::Menu(target, _)) => target.name,
                            _ => return,
                        };
                        this.focus_layout(window, cx);
                        this.request_queue_replace(
                            super::queue_replace::QueueReplacement::Playlist { id, name },
                            cx,
                        );
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
                            editor::PlaylistNameAction::Rename {
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
        let PlaylistAction::Delete(target) = action else {
            unreachable!()
        };
        let id = target.id;
        let title = self.i18n.format(
            "gl-confirm-delete-playlist",
            &[("name", target.name.as_str().into())],
        );
        let detail = self.i18n.text("gl-delete-playlist-note").to_owned();
        crate::gui::components::modal_backdrop("playlist-action-modal")
            .map(|surface| self.dialog_surface(surface))
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
                                    "󰆴",
                                    self.tr("gl-delete-playlist"),
                                    cx,
                                    move |this, window, cx| {
                                        this.dismiss_playlist_action(window, cx);
                                        if this.browser.playlists().selected() == Some(id) {
                                            this.browser.remove_playlist_selection();
                                        }
                                        this.send(Command::DeletePlaylist { playlist_id: id }, cx);
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
            self.browser.playlists().entries().rows.error(0)
        } else {
            self.browser.playlists().summaries().rows.error(0)
        };
        let pending = self.browser.requests.pending(kind);
        let playlist_id = self.browser.playlists().selected();
        let message = if let Some(error) = rows_error {
            error
        } else if pending {
            self.i18n.text("gl-loading")
        } else if (kind == ViewRequestKind::Entries
            && self.browser.playlists().entries().query.is_some()
            && self
                .browser
                .playlists()
                .selected_total()
                .is_some_and(|total| total > 0))
            || (kind == ViewRequestKind::Summaries
                && self.browser.playlists().summaries().query.is_some()
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
                            if this.browser.playlists().selected() == playlist_id
                                && this.browser.playlists().entries().rows.error(0).is_some()
                                && let Some(id) = playlist_id
                            {
                                this.request_playlist_entries(id, 0, cx);
                            }
                        } else if this.browser.playlists().summaries().rows.error(0).is_some() {
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
        let matched = self.browser.playlists().summaries().rows.total();
        let total = self.state.library.playlist_total;
        self.playlist_search.update(cx, |search, cx| {
            if let Some(matched) = matched {
                search.set_counts(matched, total, cx);
            } else {
                search.clear_counts(cx);
            }
        });
        let matched = self.browser.playlists().entries().rows.total();
        let total = self.browser.playlists().selected_total();
        self.playlist_entries_search.update(cx, |search, cx| {
            if let (Some(matched), Some(total)) = (matched, total) {
                search.set_counts(matched, total, cx);
            } else {
                search.clear_counts(cx);
            }
        });
        let row_height = list_row_height(window);
        let playlist_id = self.browser.playlists().selected();
        let selection_entry = self.browser.playlists().selected_entry();
        let selection_unavailable = !self.browser.playlists().track_selection.ready();
        let selection_pending = self.browser.playlists().track_selection.busy;
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
                            if this.browser.playlists().selected() == playlist_id
                                && this.browser.playlists().selected_entry() == selection_entry
                                && !this.browser.playlists().track_selection.ready()
                                && !this.browser.playlists().track_selection.busy
                            {
                                this.refresh_entry_selection(cx);
                                cx.notify();
                            }
                        },
                    ))
                },
            ));
        let catalog_len = self.browser.playlists().summaries().rows.len();
        self.scroll_views.register_list(
            ("playlist-catalog", panel_id),
            &self.browser.playlists().summaries().scroll,
        );
        let catalog = uniform_list(
            ("playlist-catalog", panel_id),
            catalog_len,
            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                range
                    .map(|index| {
                        let Some(p) = this.browser.playlists().summaries().rows.get(index) else {
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
                            this.browser.playlists().selected() == Some(id),
                        )
                        .when(
                            this.layout_focus.is_focused(window)
                                && this.browser.focus() == Some(super::BrowserFocus::Playlists)
                                && this.browser.playlists().selected() == Some(id),
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
                                        this.focus_layout(window, cx);
                                        this.request_queue_replace(
                                            super::queue_replace::QueueReplacement::Playlist {
                                                id,
                                                name: play_name.to_string(),
                                            },
                                            cx,
                                        );
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
        .track_scroll(&self.browser.playlists().summaries().scroll);
        let catalog = if catalog_len == 0 {
            self.empty_playlist_view(ViewRequestKind::Summaries, panel_id, cx)
                .into_any_element()
        } else {
            scrollable_list(
                ("playlist-catalog-scrollbar", panel_id),
                catalog,
                &self.browser.playlists().summaries().scroll,
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
                &self.browser.playlists().entries().scroll,
            );
            let entries_len = self.browser.playlists().entries().rows.len();
            let entries = uniform_list(
                ("playlist-entries", panel_id),
                entries_len,
                cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                    range
                        .map(|index| {
                            let Some(e) = this.browser.playlists().entries().rows.get(index) else {
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
                                this.browser.playlists().selection_generation();
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
                                this.browser.playlists().track_selection.contains(id),
                            )
                            .when(
                                this.layout_focus.is_focused(window)
                                    && this.browser.focus()
                                        == Some(super::BrowserFocus::List(
                                            ListFocus::PlaylistTracks,
                                        ))
                                    && this.browser.playlists().selected_entry() == Some(id),
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
                                    this.browser.playlists().track_selection.contains(id),
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
                                                if this.browser.playlists().selected()
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
                                        if this.browser.playlists().track_selection.contains(id)
                                            && !this.browser.playlists().track_selection.ready()
                                        {
                                            cx.stop_propagation();
                                            return;
                                        }
                                        if !this.browser.playlists().track_selection.contains(id) {
                                            this.select_playlist_entry(id, index, false, false, cx);
                                        }
                                        let rows =
                                            this.browser.playlists().track_selection.ordered();
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
                                    && this.browser.playlists().entries().query.is_none(),
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
                                                            .playlists()
                                                            .selection_generation()
                                                            != drag.selection_generation
                                                        || this
                                                            .browser
                                                            .playlists()
                                                            .entries()
                                                            .query
                                                            .is_some()
                                                        || !this
                                                            .browser
                                                            .playlists()
                                                            .track_selection
                                                            .ready()
                                                    {
                                                        return Vec::new();
                                                    }
                                                    if !this
                                                        .browser
                                                        .playlists()
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
                                                        .playlists()
                                                        .track_selection
                                                        .contains(drag.source_id)
                                                    {
                                                        this.browser
                                                            .playlists()
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
                                                        .playlists()
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
            .track_scroll(&self.browser.playlists().entries().scroll);
            let entries = if entries_len == 0 {
                self.empty_playlist_view(ViewRequestKind::Entries, panel_id, cx)
                    .into_any_element()
            } else {
                scrollable_list(
                    ("playlist-entries-scrollbar", panel_id),
                    entries,
                    &self.browser.playlists().entries().scroll,
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
                if let Some(id) = this.browser.playlists().selected() {
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
                        app.browser.playlists().selected().is_some_and(|id| {
                            app.can_transfer_content(value, super::DragDestination::Playlist(id))
                        })
                    })
                }
            })
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .on_drop(cx.listener(|this, drag: &super::ContentDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists().selected() {
                    this.drop_content(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &QueueDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists().selected() {
                    this.transfer_queue_drag(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists().selected() {
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
                        list_panel_header(
                            theme,
                            self.tr("gl-playlists"),
                            if let Some(total) = self.browser.playlists().summaries().rows.total() {
                                self.i18n
                                    .format(
                                        "gl-playlist-count",
                                        &[
                                            ("total", total.into()),
                                            (
                                                "selected",
                                                self.browser
                                                    .playlists()
                                                    .track_selection
                                                    .len()
                                                    .into(),
                                            ),
                                        ],
                                    )
                                    .into()
                            } else {
                                self.tr("gl-loading")
                            },
                        )
                        .flex_1()
                        .min_w(gpui::rems(3.)),
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
                                editor::PlaylistNameAction::Create {
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
