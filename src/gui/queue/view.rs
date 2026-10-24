use super::{Queue, QueueEvent, QueueIdentity};
use crate::{
    gui::{
        DragDestination,
        components::{
            search::set_search_counts,
            track::{favorite_button, track_list_row},
        },
        format_time,
        library::{LibraryDrag, LibraryNode},
        list_interaction::{ContentDrag, ContentMenuRequest, TrackSource},
        playlists::PlaylistDrag,
    },
    model::{Command, ScanPhase},
};
use gpui::{
    AnyElement, Context, Div, EntityId, ExternalPaths, SharedString, Window, prelude::*, px, rgb,
};
use rivu_ui::{
    IconButton, IconButtonDensity, IconButtonGroup, IconButtonStyle, StatusKind, VirtualList,
    button, caption, column, hover_actions, list_panel_header, list_viewport, panel_surface,
    panel_toolbar, row, status_message,
};
use std::cell::RefCell;

pub(in crate::gui) struct QueueDrag {
    owner: EntityId,
    source: QueueIdentity,
    // GPUI retains this payload; the drag constructor only creates its preview.
    entries: std::cell::OnceCell<Vec<QueueIdentity>>,
    revision: u64,
}

impl QueueDrag {
    pub(in crate::gui) fn can_transfer(
        &self,
        queue_revision: u64,
        destination: DragDestination,
    ) -> bool {
        destination.playlist_id().is_some()
            && self.revision == queue_revision
            && self.entries.get().is_some_and(|rows| !rows.is_empty())
    }

    pub(in crate::gui) fn track_ids(&self) -> Vec<i64> {
        self.entries
            .get()
            .unwrap()
            .iter()
            .map(|row| row.track_id)
            .collect()
    }

    fn insertion_index(
        &self,
        target: Option<(QueueIdentity, bool)>,
        total: usize,
    ) -> Option<usize> {
        let entries = self.entries.get()?;
        if entries.is_empty()
            || target.is_some_and(|(target, _)| entries.iter().any(|row| row.id == target.id))
        {
            return None;
        }
        let position = target.map_or(total, |(target, after)| {
            target.position + usize::from(after)
        });
        position.checked_sub(entries.iter().filter(|row| row.position < position).count())
    }
}

impl Queue {
    fn send_queue_row_action(&mut self, revision: u64, command: Command, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.state.queue.revision == revision {
            self.send(command, cx);
        }
    }

    pub(in crate::gui) fn selected_queue_ids_in_order(&self) -> Vec<u64> {
        self.model.ordered_ids()
    }

    fn queue_drop_index(
        &self,
        drag: &QueueDrag,
        target_id: Option<(u64, bool)>,
        owner: gpui::EntityId,
    ) -> Option<usize> {
        if self.model.query.is_some()
            || drag.owner != owner
            || drag.revision != self.state.queue.revision
        {
            return None;
        }
        let target = match target_id {
            Some((id, after)) => Some((
                self.model
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .map(QueueIdentity::from)?,
                after,
            )),
            None => None,
        };
        drag.insertion_index(target, self.state.queue.total)
    }

    fn move_queue_entry(
        &mut self,
        drag: &QueueDrag,
        target_id: Option<(u64, bool)>,
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

    fn drop_external_paths(
        &mut self,
        paths: &ExternalPaths,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        let (index, expected_revision) = match destination {
            DragDestination::Queue => (None, None),
            DragDestination::QueueAt { index, revision }
                if self.model.query.is_none() && destination.current(&self.state) =>
            {
                (Some(index), Some(revision))
            }
            _ => return,
        };
        self.send(
            Command::QueueExternalPaths {
                paths: paths.paths().to_vec(),
                index,
                expected_revision,
            },
            cx,
        );
    }

    fn queue_scan_status(&self, panel_id: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let system = &self.state.system;
        let progress = system.scan_progress.as_ref();
        let header_text = progress.map_or_else(
            || self.context.i18n.text("gl-scanning").to_owned(),
            |progress| {
                let phase = self.context.i18n.text(match progress.phase {
                    ScanPhase::Discovering => "gl-scan-discovering",
                    ScanPhase::ReadingMetadata => "gl-scan-metadata",
                    ScanPhase::Saving => "gl-scan-saving",
                    ScanPhase::Completed => "gl-scan-completed",
                    ScanPhase::Failed => "gl-scan-failed",
                    ScanPhase::Cancelled => "gl-scan-cancelled",
                });
                match progress.total {
                    Some(total) => self.context.i18n.format(
                        "gl-scan-progress",
                        &[
                            ("phase", phase.into()),
                            ("processed", progress.processed.into()),
                            ("total", total.into()),
                            ("errors", progress.errors.into()),
                        ],
                    ),
                    None => self.context.i18n.format(
                        "gl-scan-found",
                        &[
                            ("phase", phase.into()),
                            ("processed", progress.processed.into()),
                            ("errors", progress.errors.into()),
                        ],
                    ),
                }
            },
        );
        let failed = progress.is_some_and(|progress| progress.phase == ScanPhase::Failed);
        let dismissible = !system.scanning
            && progress.is_some_and(|progress| {
                matches!(
                    progress.phase,
                    ScanPhase::Completed | ScanPhase::Failed | ScanPhase::Cancelled
                )
            });
        let task_id = system.scan_task_id;
        let cancellable = system.scanning
            && task_id.is_some()
            && !progress.is_some_and(|progress| progress.phase == ScanPhase::Saving);
        let actions = gpui::div()
            .min_w_8()
            .min_h_8()
            .flex_shrink_0()
            .when(system.scanning, |slot| {
                slot.child(
                    IconButton::new(
                        ("queue-cancel-scan", panel_id),
                        "",
                        self.tr("library-cancel-task"),
                    )
                    .enabled(cancellable)
                    .build(
                        IconButtonStyle::Quiet,
                        IconButtonDensity::Regular,
                        cx,
                        move |this, _, cx| {
                            if let Some(id) = task_id {
                                this.send(Command::CancelScan { id }, cx);
                            }
                        },
                    ),
                )
            })
            .when(dismissible, |slot| {
                slot.child(
                    IconButton::new(
                        ("queue-dismiss-scan", panel_id),
                        "",
                        self.tr("gl-dismiss-scan"),
                    )
                    .build(
                        IconButtonStyle::Quiet,
                        IconButtonDensity::Regular,
                        cx,
                        |this, _, cx| this.send(Command::DismissScanResult, cx),
                    ),
                )
            });
        let mut bar = gpui::div()
            .w_full()
            .h(px(3.))
            .flex_shrink_0()
            .bg(rgb(theme.border));
        if let Some(progress) = progress
            && let Some(total) = progress.total.filter(|total| *total > 0)
        {
            let fraction = (progress.processed as f32 / total as f32).clamp(0., 1.);
            bar = bar.child(
                gpui::div()
                    .h_full()
                    .w(gpui::relative(fraction))
                    .bg(rgb(if failed { theme.error } else { theme.accent })),
            );
        }
        let path: SharedString = progress
            .and_then(|progress| progress.path.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let message: SharedString = system.scan_message.clone().into();
        column()
            .gap_1()
            .min_w_0()
            .flex_shrink_0()
            .child(
                row()
                    .min_h_8()
                    .flex_shrink_0()
                    .child(caption(theme, header_text).flex_1().min_w_0().truncate())
                    .child(actions),
            )
            .child(bar)
            .child(
                caption(theme, path.clone())
                    .id(("queue-scan-path", panel_id))
                    .min_h_5()
                    .flex_shrink_0()
                    .min_w_0()
                    .truncate()
                    .tooltip(rivu_ui::text_tooltip(path)),
            )
            .child(
                status_message(
                    theme,
                    message.clone(),
                    if failed {
                        StatusKind::Error
                    } else {
                        StatusKind::Info
                    },
                )
                .id(("queue-scan-message", panel_id))
                .min_h_5()
                .flex_shrink_0()
                .min_w_0()
                .truncate()
                .tooltip(rivu_ui::text_tooltip(message)),
            )
    }

    pub(in crate::gui) fn render_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let matched = self.model.rows.total();
        let total = self.state.queue.total;
        self.queue_search.update(cx, |search, cx| {
            if let Some(matched) = matched {
                set_search_counts(search, matched, total, &self.context.i18n, cx);
            } else {
                search.clear_counts(cx);
            }
        });
        let selection_count = self.model.len();
        let header = list_panel_header(
            theme,
            self.tr("gl-queue"),
            self.context.i18n.format(
                "gl-queue-count",
                &[
                    ("total", (self.state.queue.total).into()),
                    ("selected", (selection_count).into()),
                ],
            ),
        )
        .id(("queue-count", panel_id))
        .tooltip({
            let text = self.tr("gl-queue-drag-hint");
            rivu_ui::text_tooltip(text)
        });
        let tools = rivu_ui::panel_header_actions()
            .child(self.queue_search.clone())
            .child(
                IconButton::new(
                    ("queue-locate-playing", panel_id),
                    "",
                    self.tr("gl-locate-playing-entry"),
                )
                .enabled(self.state.queue.current_id.is_some())
                .build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    |this, window, cx| this.locate_playing_queue(window, cx),
                ),
            )
            .min_w_0()
            .max_w(gpui::rems(20.5))
            .flex_shrink(1.);
        let (selection_message, selection_kind): (SharedString, _) =
            if let Some(error) = &self.model.selection_error {
                (error.clone().into(), StatusKind::Error)
            } else if self.model.busy {
                (
                    self.context.i18n.text("gl-selection-loading").into(),
                    StatusKind::Info,
                )
            } else {
                ("".into(), StatusKind::Info)
            };
        let status = row()
            .min_h_8()
            .flex_shrink_0()
            .overflow_hidden()
            .child(
                status_message(theme, selection_message.clone(), selection_kind)
                    .id(("queue-selection-status", panel_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(rivu_ui::text_tooltip(selection_message)),
            )
            .when(self.model.selection_error.is_some(), |status| {
                status.child(
                    IconButton::new(
                        ("queue-selection-retry", panel_id),
                        "󰑐",
                        self.tr("gl-retry-selection"),
                    )
                    .build(
                        IconButtonStyle::Surface,
                        IconButtonDensity::Regular,
                        cx,
                        |this, _, cx| {
                            if this.model.selection_error.is_some() {
                                this.refresh_queue(cx);
                                cx.notify();
                            }
                        },
                    ),
                )
            });
        let scroll = self.context.scroll.borrow_mut().list(("queue", panel_id));
        self.reveal_queue_locate(&scroll, cx);
        let panel_drop_owner = cx.entity().downgrade();
        let list_drop_owner = panel_drop_owner.clone();
        panel_surface(("queue-panel", panel_id))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.drop_external_paths(paths, DragDestination::Queue, cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgb(theme.accent)))
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
                    } else if value.is::<ExternalPaths>() {
                        true
                    } else if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
                        this.can_resolve_library_drag(drag)
                    } else {
                        this.can_transfer_content(value, DragDestination::Queue)
                    }
                })
            })
            .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                this.resolve_library_drag(drag.nodes(), DragDestination::Queue, cx);
            }))
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.drop_content(drag, DragDestination::Queue, cx);
            }))
            .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.transfer_playlist_drag(drag, DragDestination::Queue, cx);
            }))
            .drag_over::<ContentDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .drag_over::<PlaylistDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .child(rivu_ui::panel_heading_row(header, tools))
            .when(
                self.model
                    .locate_filtered
                    .is_some_and(|id| Some(id) == self.state.queue.current_id),
                |panel| {
                    panel.child(
                        button(
                            ("queue-clear-and-locate", panel_id),
                            self.tr("search-clear-and-locate"),
                            cx,
                            |this, window, cx| {
                                this.queue_search.update(cx, |search, cx| search.clear(cx));
                                this.apply_queue_search(None, cx);
                                this.locate_playing_queue(window, cx);
                            },
                        )
                        .self_start(),
                    )
                },
            )
            .when(matched == Some(0), |panel| {
                panel.child(caption(
                    theme,
                    self.tr(if total == 0 {
                        "search-empty"
                    } else {
                        "search-no-matches"
                    }),
                ))
            })
            .when(
                self.model.busy || self.model.selection_error.is_some(),
                |panel| panel.child(status),
            )
            .when(
                self.state.system.scanning || self.state.system.scan_progress.is_some(),
                |panel| panel.child(self.queue_scan_status(panel_id, cx)),
            )
            .child(list_viewport(
                VirtualList::new(
                    ("queue-rows", panel_id),
                    self.model.rows.len(),
                    &scroll,
                    cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                        range
                            .map(|index| {
                                let Some(entry) = this.model.rows.get(index) else {
                                    return this
                                        .paged_loading_row(index, cx)
                                        .h(rivu_ui::list_row_height(window, cx));
                                };
                                let id = entry.id;
                                let track_id = entry.track_id;
                                let favorite = entry.track.favorite;
                                let position = entry.position;
                                let can_move_down = position + 1 < this.state.queue.total;
                                let revision = this.state.queue.revision;
                                let (title, detail) = this.panel_track_text(&entry.track);
                                let playing = this.state.queue.current_id == Some(id);
                                let drag_owner = cx.entity().downgrade();
                                let drag_i18n = std::sync::Arc::clone(&this.context.i18n);
                                let drag = QueueDrag {
                                    owner: cx.entity().entity_id(),
                                    source: QueueIdentity::from(entry),
                                    entries: Default::default(),
                                    revision: this.state.queue.revision,
                                };
                                track_list_row(
                                    theme,
                                    ("queue-entry", id),
                                    ("queue-entry-text", id),
                                    (title, detail),
                                    position,
                                    playing,
                                    this.model.contains(&id),
                                )
                                .h(rivu_ui::list_row_height(window, cx))
                                .when(this.queue_locate_emphasized(id), |row| {
                                    row.border_1()
                                        .border_color(rgb(theme.accent))
                                        .bg(rgb(theme.highlight))
                                })
                                .when(
                                    this.focused
                                        && this.model.active().is_some_and(|row| row.id == id),
                                    |row| row.border_color(rgb(theme.accent)),
                                )
                                .group("queue-track-row")
                                .relative()
                                .child(
                                    hover_actions(
                                        theme,
                                        ("queue-entry-actions", id),
                                        "queue-track-row",
                                        this.model.contains(&id),
                                    )
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        IconButtonGroup::new(
                                            IconButtonStyle::Quiet,
                                            IconButtonDensity::Compact,
                                        )
                                        .button(
                                            IconButton::new(
                                                ("queue-entry-play", id),
                                                "",
                                                this.tr("gl-play-queue-entry"),
                                            ),
                                            cx,
                                            move |this, _, cx| {
                                                this.send_queue_row_action(
                                                    revision,
                                                    Command::PlayQueue { queue_id: id },
                                                    cx,
                                                )
                                            },
                                        )
                                        .button(
                                            favorite_button(
                                                ("queue-entry-favorite", id),
                                                favorite,
                                                |key| this.tr(key),
                                            ),
                                            cx,
                                            move |this, _, cx| {
                                                this.send(
                                                    Command::SetFavorite {
                                                        track_ids: vec![track_id],
                                                        favorite: !favorite,
                                                    },
                                                    cx,
                                                )
                                            },
                                        )
                                        .button(
                                            IconButton::new(
                                                ("queue-entry-remove", id),
                                                "",
                                                this.tr("gl-remove-queue"),
                                            ),
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
                                        )
                                        .button(
                                            IconButton::new(
                                                ("queue-entry-up", id),
                                                "",
                                                this.tr("gl-entry-up"),
                                            )
                                            .enabled(position > 0),
                                            cx,
                                            move |this, _, cx| {
                                                if position > 0 {
                                                    this.send_queue_row_action(
                                                        revision,
                                                        Command::MoveQueue {
                                                            queue_id: id,
                                                            index: position - 1,
                                                        },
                                                        cx,
                                                    );
                                                }
                                            },
                                        )
                                        .button(
                                            IconButton::new(
                                                ("queue-entry-down", id),
                                                "",
                                                this.tr("gl-entry-down"),
                                            )
                                            .enabled(can_move_down),
                                            cx,
                                            move |this, _, cx| {
                                                if can_move_down {
                                                    this.send_queue_row_action(
                                                        revision,
                                                        Command::MoveQueue {
                                                            queue_id: id,
                                                            index: position + 1,
                                                        },
                                                        cx,
                                                    );
                                                }
                                            },
                                        )
                                        .into_div(),
                                    ),
                                )
                                .on_mouse_down(
                                    gpui::MouseButton::Right,
                                    cx.listener(
                                        move |this, event: &gpui::MouseDownEvent, _, cx| {
                                            cx.emit(QueueEvent::FocusRequested);
                                            if this.model.contains(&id) && !this.model.ready() {
                                                cx.stop_propagation();
                                                return;
                                            }
                                            if !this.model.contains(&id) {
                                                this.select_queue_entry(id, false, false, cx);
                                                cx.notify();
                                            }
                                            let rows = this.model.ordered();
                                            let track_ids = if rows.iter().any(|row| row.id == id) {
                                                rows.iter().map(|row| row.track_id).collect()
                                            } else {
                                                vec![track_id]
                                            };
                                            let queue_ids = if rows.iter().any(|row| row.id == id) {
                                                rows.iter().map(|row| row.id).collect()
                                            } else {
                                                vec![id]
                                            };
                                            cx.emit(QueueEvent::ContentMenu {
                                                request: std::cell::RefCell::new(Some(
                                                    ContentMenuRequest {
                                                        source: TrackSource::Tracks(track_ids),
                                                        position: event.position,
                                                        play: Some(Command::PlayQueue {
                                                            queue_id: id,
                                                        }),
                                                    },
                                                )),
                                                revision,
                                                remove: std::cell::RefCell::new(Some(
                                                    Command::RemoveQueueEntries { queue_ids },
                                                )),
                                            });
                                            cx.stop_propagation();
                                        },
                                    ),
                                )
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, _, cx| {
                                        cx.emit(QueueEvent::FocusRequested);
                                        let modifiers = event.modifiers();
                                        this.select_queue_entry(
                                            id,
                                            modifiers.shift,
                                            modifiers.control,
                                            cx,
                                        );
                                        if event.click_count() == 2 {
                                            this.send(Command::PlayQueue { queue_id: id }, cx);
                                        }
                                    },
                                ))
                                .when(
                                    this.model.ready()
                                        && this.model.query.is_none()
                                        && this.model.selection_error.is_none(),
                                    |row| {
                                        row.on_drag(drag, move |drag, _, window, cx| {
                                            rivu_ui::begin_list_drag(window, cx);
                                            drag.entries.get_or_init(|| {
                                                drag_owner
                                                    .update(cx, |this, cx| {
                                                        this.refresh(cx);
                                                        if drag.revision
                                                            != this.state.queue.revision
                                                            || !this.model.ready()
                                                            || this.model.query.is_some()
                                                            || this.model.selection_error.is_some()
                                                        {
                                                            return Vec::new();
                                                        }
                                                        if this.model.contains(&drag.source.id) {
                                                            this.model.ordered()
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
                                            cx.new(|_| rivu_ui::ListDragPreview { label })
                                        })
                                    },
                                )
                                .children([false, true].map(|after| {
                                    this.queue_row_drop_zone(
                                        (id, position),
                                        revision,
                                        after,
                                        theme,
                                        cx,
                                    )
                                }))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .on_viewport_deferred(cx.weak_entity(), Self::ensure_queue_range)
                .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.drop_external_paths(paths, DragDestination::Queue, cx);
                }))
                .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgb(theme.accent)))
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
                        } else if value.is::<ExternalPaths>() {
                            true
                        } else if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
                            this.can_resolve_library_drag(drag)
                        } else {
                            this.can_transfer_content(value, DragDestination::Queue)
                        }
                    })
                })
                .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
                .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                    window.prevent_default();
                    this.resolve_library_drag(drag.nodes(), DragDestination::Queue, cx);
                }))
                .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
                .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.drop_content(drag, DragDestination::Queue, cx);
                }))
                .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.transfer_playlist_drag(drag, DragDestination::Queue, cx);
                }))
                .drag_over::<ContentDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
                .drag_over::<PlaylistDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
                .h_full()
                .min_h_0()
                .w_full()
                .scrollbar(("queue-scrollbar", panel_id)),
            ))
            .into_any_element()
    }
    fn queue_row_drop_valid(
        &self,
        value: &dyn std::any::Any,
        target: u64,
        after: bool,
        destination: DragDestination,
        owner: EntityId,
    ) -> bool {
        if self.model.query.is_some() || !destination.current(&self.state) {
            return false;
        }
        if let Some(drag) = value.downcast_ref::<QueueDrag>() {
            self.queue_drop_index(drag, Some((target, after)), owner)
                .is_some()
        } else if value.is::<ExternalPaths>() {
            true
        } else if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
            self.can_resolve_library_drag(drag)
        } else {
            self.can_transfer_content(value, destination)
        }
    }

    fn queue_row_drop_zone(
        &self,
        (id, position): (u64, usize),
        revision: u64,
        after: bool,
        theme: &'static rivu_ui::theme::Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let destination = DragDestination::QueueAt {
            index: position + usize::from(after),
            revision,
        };
        let owner = cx.weak_entity();
        macro_rules! indicator {
            ($zone:expr, $payload:ty) => {{
                let owner = owner.clone();
                $zone.drag_over::<$payload>(move |style, drag, _, cx| {
                    if owner.upgrade().is_some_and(|owner| {
                        owner.read(cx).queue_row_drop_valid(
                            drag,
                            id,
                            after,
                            destination,
                            owner.entity_id(),
                        )
                    }) {
                        style.border_color(rgb(theme.accent))
                    } else {
                        style
                    }
                })
            }};
        }
        let zone = rivu_ui::list_drop_target(
            (
                if after {
                    "queue-insert-after"
                } else {
                    "queue-insert-before"
                },
                id,
            ),
            after,
        )
        .can_drop(|value, _, _| {
            value.is::<QueueDrag>()
                || value.is::<LibraryDrag>()
                || value.is::<ContentDrag>()
                || value.is::<PlaylistDrag>()
                || value.is::<ExternalPaths>()
        })
        .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
            // Consume rejected positional drops instead of falling back to append.
            this.drop_external_paths(paths, destination, cx);
        }))
        .on_drop(cx.listener(move |this, drag: &QueueDrag, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
            this.refresh(cx);
            if this.queue_row_drop_valid(drag, id, after, destination, cx.entity().entity_id()) {
                this.move_queue_entry(drag, Some((id, after)), cx);
            }
        }))
        .on_drop(cx.listener(move |this, drag: &LibraryDrag, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
            this.refresh(cx);
            if this.queue_row_drop_valid(drag, id, after, destination, cx.entity().entity_id()) {
                this.resolve_library_drag(drag.nodes(), destination, cx);
            }
        }))
        .on_drop(cx.listener(move |this, drag: &ContentDrag, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
            this.refresh(cx);
            if this.queue_row_drop_valid(drag, id, after, destination, cx.entity().entity_id()) {
                this.drop_content(drag, destination, cx);
            }
        }))
        .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
            this.refresh(cx);
            if this.queue_row_drop_valid(drag, id, after, destination, cx.entity().entity_id()) {
                this.transfer_playlist_drag(drag, destination, cx);
            }
        }));
        let zone = indicator!(zone, QueueDrag);
        let zone = indicator!(zone, LibraryDrag);
        let zone = indicator!(zone, ContentDrag);
        let zone = indicator!(zone, ExternalPaths);
        indicator!(zone, PlaylistDrag)
    }
}

impl Queue {
    pub(super) fn can_resolve_library_drag(&self, drag: &LibraryDrag) -> bool {
        drag.can_resolve(&self.state, self.library_selection_pending)
    }

    pub(super) fn can_transfer_content(
        &self,
        value: &dyn std::any::Any,
        destination: DragDestination,
    ) -> bool {
        if !destination.current(&self.state) {
            return false;
        }
        if let Some(drag) = value.downcast_ref::<ContentDrag>() {
            return match drag.source() {
                Some(TrackSource::Tracks(ids)) => !ids.is_empty(),
                Some(TrackSource::Album(_)) => !self.content_resolution,
                None => false,
            };
        }
        value
            .downcast_ref::<PlaylistDrag>()
            .is_some_and(|drag| drag.can_transfer(&self.state, destination))
    }

    pub(super) fn resolve_library_drag(
        &mut self,
        nodes: &[LibraryNode],
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.library_selection_pending && !nodes.is_empty() && destination.current(&self.state)
        {
            cx.emit(QueueEvent::DropLibrary(
                RefCell::new(Some(nodes.to_vec())),
                destination,
            ));
        }
    }

    pub(super) fn drop_content(
        &mut self,
        drag: &ContentDrag,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self.can_transfer_content(drag, destination)
            && let Some(source) = drag.source()
        {
            cx.emit(QueueEvent::DropContent(
                RefCell::new(Some(source.clone())),
                destination,
            ));
        }
    }

    pub(super) fn transfer_playlist_drag(
        &mut self,
        drag: &PlaylistDrag,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if drag.can_transfer(&self.state, destination) {
            cx.emit(QueueEvent::DropPlaylist(
                RefCell::new(Some(drag.track_ids())),
                destination,
            ));
        }
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
                self.context.i18n.text("gl-file-missing-suffix")
            } else {
                ""
            }
        );
        (track.title.clone(), detail)
    }

    pub(super) fn paged_loading_row(
        &self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let error = self.model.rows.error(index);
        rivu_ui::loading::paged_loading_row(
            ("loading-row", index),
            error,
            self.loading.show(&(), error.is_some()),
            rivu_ui::loading::LoadingRowLabels {
                message: self.tr(if error.is_some() {
                    "shell-loading-failed"
                } else {
                    "shell-loading"
                }),
                retry: self.tr("gl-retry-loading"),
                copy_error: self.tr("shell-copy-message"),
            },
            cx,
            |this, _, cx| {
                this.retry_queue_page(cx);
                cx.notify();
            },
        )
    }
}
#[cfg(test)]
mod queue_drag_tests {
    use super::{QueueDrag, QueueIdentity};

    fn identity(position: usize) -> QueueIdentity {
        QueueIdentity {
            id: position as u64 + 10,
            track_id: 7,
            position,
            offset: position,
        }
    }

    fn drag(positions: &[usize]) -> QueueDrag {
        QueueDrag {
            owner: 1_u64.into(),
            source: identity(positions[0]),
            entries: std::cell::OnceCell::from(
                positions.iter().copied().map(identity).collect::<Vec<_>>(),
            ),
            revision: 4,
        }
    }

    fn reordered(drag: &QueueDrag, target: Option<usize>) -> Vec<u64> {
        reordered_at(drag, target, false)
    }

    fn reordered_at(drag: &QueueDrag, target: Option<usize>, after: bool) -> Vec<u64> {
        let index = drag
            .insertion_index(target.map(|position| (identity(position), after)), 5)
            .unwrap();
        let selected: Vec<_> = drag
            .entries
            .get()
            .unwrap()
            .iter()
            .map(|row| row.id)
            .collect();
        let mut remaining: Vec<_> = (0..5)
            .map(identity)
            .map(|row| row.id)
            .filter(|id| !selected.contains(id))
            .collect();
        remaining.splice(index..index, selected);
        remaining
    }

    #[test]
    fn queue_transfer_requires_current_initialized_occurrences_and_playlist_destination() {
        let selected = drag(&[1, 3]);
        assert!(selected.can_transfer(4, super::DragDestination::Playlist(7)));
        assert!(!selected.can_transfer(5, super::DragDestination::Playlist(7)));
        assert!(!selected.can_transfer(4, super::DragDestination::Queue));
        assert_eq!(selected.track_ids(), vec![7, 7]);
        let pending = QueueDrag {
            owner: 1_u64.into(),
            source: identity(1),
            entries: Default::default(),
            revision: 4,
        };
        assert!(!pending.can_transfer(4, super::DragDestination::Playlist(7)));
        let empty = QueueDrag {
            entries: std::cell::OnceCell::from(Vec::new()),
            ..pending
        };
        assert!(!empty.can_transfer(4, super::DragDestination::Playlist(7)));
    }

    #[test]
    fn queue_drop_inserts_before_target_or_appends_in_source_order() {
        assert_eq!(reordered(&drag(&[1]), Some(4)), [10, 12, 13, 11, 14]);
        assert_eq!(reordered(&drag(&[3]), Some(1)), [10, 13, 11, 12, 14]);
        assert_eq!(reordered(&drag(&[1]), None), [10, 12, 13, 14, 11]);
        // All rows share a track ID; distinct queue occurrences must survive.
        let group = drag(&[1, 3]);
        assert_eq!(reordered(&group, Some(4)), [10, 12, 11, 13, 14]);
        assert_eq!(reordered(&group, Some(0)), [11, 13, 10, 12, 14]);
        assert_eq!(reordered(&group, None), [10, 12, 14, 11, 13]);
        assert_eq!(group.insertion_index(Some((identity(1), false)), 5), None);
        assert_eq!(group.insertion_index(Some((identity(3), true)), 5), None);
    }

    #[test]
    fn queue_drop_after_accounts_for_removed_occurrences() {
        assert_eq!(
            reordered_at(&drag(&[1]), Some(3), true),
            [10, 12, 13, 11, 14]
        );
        assert_eq!(
            reordered_at(&drag(&[3]), Some(0), true),
            [10, 13, 11, 12, 14]
        );
        assert_eq!(
            reordered_at(&drag(&[1, 3]), Some(2), true),
            [10, 12, 11, 13, 14]
        );
        assert_eq!(
            reordered_at(&drag(&[0, 1]), Some(4), true),
            [12, 13, 14, 10, 11]
        );
    }
}
