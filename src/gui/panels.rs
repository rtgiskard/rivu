use super::{ACCENT, BORDER, Field, GuiApp, MUTED, PANEL, TEXT, button, column, format_time, row};
use crate::model::{Command, RepeatMode};
use gpui::{
    AnyElement, Context, Div, ElementId, Render, SharedString, Stateful, Window, div, prelude::*,
    px, rgb, uniform_list,
};
use std::path::PathBuf;

const TRACK_HEIGHT: f32 = 42.0;

fn caption(text: impl Into<gpui::SharedString>) -> Div {
    div().text_xs().text_color(rgb(MUTED)).child(text.into())
}

fn list_row(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    row()
        .id(id)
        .w_full()
        .h(px(TRACK_HEIGHT))
        .flex_shrink_0()
        .min_w_0()
        .px_2()
        .rounded_sm()
        .overflow_hidden()
        .cursor_pointer()
        .bg(rgb(if selected { 0x293d40 } else { PANEL }))
        .hover(|style| style.bg(rgb(0x293039)))
}

struct TrackTooltip {
    title: SharedString,
    detail: SharedString,
}

impl Render for TrackTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        column()
            .max_w(px(560.))
            .px_3()
            .py_2()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(TEXT))
            .child(self.title.clone())
            .child(caption(self.detail.clone()))
    }
}

fn row_text(title: String, detail: String) -> Stateful<Div> {
    let title: SharedString = title.into();
    let detail: SharedString = detail.into();
    column()
        .id("track-text")
        .flex_1()
        .gap_0()
        .overflow_hidden()
        .child(
            div()
                .text_sm()
                .text_color(rgb(TEXT))
                .truncate()
                .child(title.clone()),
        )
        .child(caption(detail.clone()).truncate())
        .tooltip(move |_, cx| {
            cx.new(|_| TrackTooltip {
                title: title.clone(),
                detail: detail.clone(),
            })
            .into()
        })
}

impl GuiApp {
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

    fn panel_track_text(&self, track_id: i64) -> (String, String) {
        match self
            .library_index
            .get(&track_id)
            .and_then(|index| self.state.library.get(*index))
        {
            Some(track) => {
                let detail = format!(
                    "{}{}{}  ·  {}{}",
                    track.artist,
                    if track.artist.is_empty() || track.album.is_empty() {
                        ""
                    } else {
                        " / "
                    },
                    track.album,
                    format_time(track.duration.unwrap_or(0.0)),
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
                this.send(Command::Scan { paths }, cx);
            },
        ));
        if !self.selected.is_empty() {
            tools = tools
                .child(button(
                    ("library-play", panel_id),
                    "Play",
                    cx,
                    |this, _, cx| {
                        if let Some(track_id) = this.panel_selected_tracks().first().copied() {
                            this.send(Command::Play { track_id }, cx);
                        }
                    },
                ))
                .child(button(
                    ("library-enqueue", panel_id),
                    "Enqueue",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::Enqueue {
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        );
                    },
                ))
                .child(button(
                    ("library-remove", panel_id),
                    "Remove",
                    cx,
                    |this, _, cx| {
                        this.send(
                            Command::RemoveTracks {
                                track_ids: this.panel_selected_tracks(),
                            },
                            cx,
                        );
                        this.selected.clear();
                        cx.notify();
                    },
                ))
                .child(button(
                    ("library-clear-selection", panel_id),
                    "Deselect",
                    cx,
                    |this, _, cx| {
                        this.selected.clear();
                        cx.notify();
                    },
                ));
        }
        let mut panel = column()
            .id(("library-panel", panel_id))
            .size_full()
            .p_3()
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
                                        move |this, event: &gpui::ClickEvent, _, cx| {
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

    fn move_selected_queue(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(queue_id) = self.selected_queue else {
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

    pub(super) fn queue_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self
            .selected_queue
            .and_then(|id| self.state.queue.iter().position(|entry| entry.id == id));
        let mut tools = row().flex_wrap().flex_shrink_0();
        if let Some(index) = selected {
            tools = tools
                .child(button(
                    ("queue-play", panel_id),
                    "Play",
                    cx,
                    |this, _, cx| {
                        if let Some(queue_id) = this.selected_queue {
                            this.send(Command::PlayQueue { queue_id }, cx);
                        }
                    },
                ))
                .child(button(
                    ("queue-remove", panel_id),
                    "Remove",
                    cx,
                    |this, _, cx| {
                        if let Some(queue_id) = this.selected_queue.take() {
                            this.send(Command::RemoveQueue { queue_id }, cx);
                        }
                    },
                ));
            if index > 0 {
                tools = tools.child(button(("queue-up", panel_id), "Up", cx, |this, _, cx| {
                    this.move_selected_queue(false, cx)
                }));
            }
            if index + 1 < self.state.queue.len() {
                tools = tools.child(button(
                    ("queue-down", panel_id),
                    "Down",
                    cx,
                    |this, _, cx| this.move_selected_queue(true, cx),
                ));
            }
        }
        if !self.state.queue.is_empty() {
            tools = tools.child(button(
                ("queue-clear", panel_id),
                "Clear queue",
                cx,
                |this, _, cx| {
                    this.selected_queue = None;
                    this.send(Command::ClearQueue, cx);
                },
            ));
        }
        column()
            .id(("queue-panel", panel_id))
            .size_full()
            .p_3()
            .child(caption(format!(
                "{} queued · double-click to play an occurrence",
                self.state.queue.len()
            )))
            .child(tools)
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
                                Some(
                                    list_row(("queue-entry", id), this.selected_queue == Some(id))
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
                                                .child(if playing {
                                                    "Now".to_owned()
                                                } else {
                                                    (index + 1).to_string()
                                                }),
                                        )
                                        .child(row_text(title, detail))
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, _, cx| {
                                                this.selected_queue = Some(id);
                                                if event.click_count() == 2 {
                                                    this.send(
                                                        Command::PlayQueue { queue_id: id },
                                                        cx,
                                                    );
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
        let mut manage = row().flex_wrap().flex_shrink_0().child(button(
            ("playlist-create", panel_id),
            "Create",
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
                .child(button(
                    ("playlist-rename", panel_id),
                    "Rename",
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
                .child(button(
                    ("playlist-delete", panel_id),
                    "Delete",
                    cx,
                    move |this, _, cx| {
                        this.selected_playlist = None;
                        this.selected_entry = None;
                        this.send(Command::DeletePlaylist { playlist_id }, cx);
                    },
                ));
        }
        let mut files = row().flex_wrap().flex_shrink_0().child(button(
            ("playlist-import", panel_id),
            "Import M3U",
            cx,
            |this, _, cx| {
                let path = this.value(Field::Path, cx);
                if path.trim().is_empty() {
                    this.panel_error("Enter the M3U file path first.", cx);
                    return;
                }
                this.send(
                    Command::ImportPlaylist {
                        path: PathBuf::from(path.trim()),
                        name: None,
                    },
                    cx,
                );
            },
        ));
        if let Some(playlist_id) = playlist_id {
            files = files.child(button(
                ("playlist-export", panel_id),
                "Export M3U",
                cx,
                move |this, _, cx| {
                    let path = this.value(Field::Path, cx);
                    if path.trim().is_empty() {
                        this.panel_error("Enter the destination M3U file path first.", cx);
                        return;
                    }
                    this.send(
                        Command::ExportPlaylist {
                            playlist_id,
                            path: PathBuf::from(path.trim()),
                        },
                        cx,
                    );
                },
            ));
        }
        let catalog_height = (self.state.playlists.len().max(1) as f32 * TRACK_HEIGHT).min(126.0);
        let mut panel = column()
            .id(("playlists-panel", panel_id))
            .size_full()
            .p_3()
            .child(self.panel_field(Field::PlaylistName, "Playlist name"))
            .child(manage)
            .child(self.panel_field(Field::Path, "M3U import / export path"))
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
                                        move |this, _: &gpui::ClickEvent, _, cx| {
                                            this.selected_playlist = Some(id);
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
                tools = tools.child(button(
                    ("playlist-play", panel_id),
                    "Play playlist",
                    cx,
                    move |this, _, cx| {
                        this.send(Command::PlayPlaylist { playlist_id }, cx);
                    },
                ));
            }
            if !self.selected.is_empty() {
                tools = tools.child(button(
                    ("playlist-add", panel_id),
                    "Add selected",
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
                    .child(button(
                        ("entry-play", panel_id),
                        "Play track",
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
                    .child(button(
                        ("entry-remove", panel_id),
                        "Remove",
                        cx,
                        |this, _, cx| {
                            if let Some(entry_id) = this.selected_entry.take() {
                                this.send(Command::RemovePlaylistEntry { entry_id }, cx);
                            }
                        },
                    ));
                if index > 0 {
                    tools = tools.child(button(("entry-up", panel_id), "Up", cx, |this, _, cx| {
                        this.move_selected_playlist_entry(false, cx)
                    }));
                }
                if index + 1 < entry_count {
                    tools = tools.child(button(
                        ("entry-down", panel_id),
                        "Down",
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
                                        .on_click(
                                            cx.listener(
                                                move |this, event: &gpui::ClickEvent, _, cx| {
                                                    this.selected_entry = Some(id);
                                                    if event.click_count() == 2 {
                                                        this.send(Command::Play { track_id }, cx);
                                                    }
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .w_full(),
                );
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
            .p_3()
            .overflow_y_scroll();
        if let Some(track) = self
            .metadata_track
            .and_then(|id| self.library_index.get(&id))
            .and_then(|index| self.state.library.get(*index))
        {
            let id = track.id;
            panel = panel
                .child(caption(format!(
                    "Track #{id} · {} · {} Hz · {} channels",
                    track.codec, track.sample_rate, track.channels
                )))
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
            .p_3()
            .child(caption(format!(
                "Recent listening · {} sessions · newest first",
                self.state.history.len()
            )))
            .when(self.state.history.is_empty(), |panel| {
                panel.child(caption("Listening sessions will appear here."))
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
                                let mut item_row =
                                    list_row(("history-entry", item.id as u64), false).child(
                                        row_text(
                                            item.title.clone(),
                                            format!(
                                                "{} listened · {} · {}",
                                                format_time(item.listened_seconds),
                                                item.reason,
                                                if item.counted {
                                                    "counted play"
                                                } else {
                                                    "not counted"
                                                }
                                            ),
                                        ),
                                    );
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
            .child(caption("Most played · ranked by completed play count"))
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
                                        format!(
                                            "{} plays · {} listened",
                                            track.play_count,
                                            format_time(track.listen_seconds)
                                        ),
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

    pub(super) fn settings_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shuffle = if self.settings_draft.shuffle {
            "Shuffle: on"
        } else {
            "Shuffle: off"
        };
        let repeat = match self.settings_draft.repeat {
            RepeatMode::Off => "Repeat: off",
            RepeatMode::All => "Repeat: all",
            RepeatMode::One => "Repeat: one",
        };
        let mpris = if self.settings_draft.mpris_enabled {
            "MPRIS: enabled"
        } else {
            "MPRIS: disabled"
        };
        let device_height = ((self.state.devices.len() + 1) as f32 * TRACK_HEIGHT).min(126.0);
        let chosen_device = self.value(Field::ConfigDevice, cx);
        column()
            .id(("settings-panel", panel_id))
            .size_full()
            .p_3()
            .overflow_y_scroll()
            .child(caption("Preferences · changes take effect when saved"))
            .child(caption(self.state.config_path.to_string_lossy().into_owned()).truncate())
            .child(
                row()
                    .flex_wrap()
                    .flex_shrink_0()
                    .child(button(
                        ("settings-save", panel_id),
                        "Save & apply",
                        cx,
                        |this, _, cx| this.apply_settings(cx),
                    ))
                    .child(button(
                        ("settings-reload", panel_id),
                        "Reload file",
                        cx,
                        |this, _, cx| this.reload_settings(cx),
                    ))
                    .child(button(
                        ("settings-revert", panel_id),
                        "Discard edits",
                        cx,
                        |this, _, cx| this.load_settings(cx),
                    )),
            )
            .child(self.panel_field(
                Field::ConfigRoots,
                "Library roots (separate paths with semicolons)",
            ))
            .child(self.panel_field(
                Field::ConfigDevice,
                "Output device (blank = system default)",
            ))
            .child(caption("Available outputs · click to select"))
            .child(
                uniform_list(
                    ("settings-devices", panel_id),
                    self.state.devices.len() + 1,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let name = if index == 0 {
                                    String::new()
                                } else {
                                    this.state.devices.get(index - 1)?.clone()
                                };
                                let label = if name.is_empty() {
                                    "System default".to_owned()
                                } else {
                                    name.clone()
                                };
                                let selected = chosen_device == name;
                                Some(
                                    list_row(("output-device", index), selected)
                                        .child(div().text_sm().truncate().child(label))
                                        .on_click(cx.listener(
                                            move |this, _: &gpui::ClickEvent, _, cx| {
                                                this.settings_draft.output_device =
                                                    (!name.is_empty()).then(|| name.clone());
                                                this.set_value(
                                                    Field::ConfigDevice,
                                                    name.clone(),
                                                    cx,
                                                );
                                                cx.notify();
                                            },
                                        )),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(px(device_height))
                .flex_shrink_0()
                .w_full(),
            )
            .child(self.panel_field(Field::ConfigVolume, "Volume (0–100%)"))
            .child(
                row()
                    .flex_wrap()
                    .flex_shrink_0()
                    .child(button(
                        ("settings-shuffle", panel_id),
                        shuffle,
                        cx,
                        |this, _, cx| {
                            this.settings_draft.shuffle = !this.settings_draft.shuffle;
                            cx.notify();
                        },
                    ))
                    .child(button(
                        ("settings-repeat", panel_id),
                        repeat,
                        cx,
                        |this, _, cx| {
                            this.settings_draft.repeat = match this.settings_draft.repeat {
                                RepeatMode::Off => RepeatMode::All,
                                RepeatMode::All => RepeatMode::One,
                                RepeatMode::One => RepeatMode::Off,
                            };
                            cx.notify();
                        },
                    )),
            )
            .child(self.panel_field(Field::ConfigScale, "Interface scale (0.75–2.0)"))
            .child(self.panel_field(Field::ConfigFps, "Analysis frames / second (5–60)"))
            .child(row().child(button(
                ("settings-mpris", panel_id),
                mpris,
                cx,
                |this, _, cx| {
                    this.settings_draft.mpris_enabled = !this.settings_draft.mpris_enabled;
                    cx.notify();
                },
            )))
            .child(caption(format!(
                "Media controls: {}",
                self.state.mpris_status
            )))
            .child(div().h(px(1.0)).flex_shrink_0().bg(rgb(BORDER)))
            .child(row().flex_wrap().child(button(
                ("settings-reset-workspace", panel_id),
                "Reset workspace",
                cx,
                |this, _, cx| {
                    this.layout = super::layout::Layout::default();
                    this.persist_layout(cx);
                    cx.notify();
                },
            )))
            .child(caption(
                "Resets panel placement only. Your music and preferences are kept.",
            ))
            .into_any_element()
    }
}
