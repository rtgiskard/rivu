//! History list rendering; all row closures target the History owner.

use super::{History, HistoryEvent, HistorySort};
use crate::gui::{
    components::{
        caption, column, compact_icon_button, hover_actions, icon_button, list_panel_header,
        list_row, list_viewport, panel_surface, row, scrollable_list, sortable_header,
    },
    fonts::tabular_numbers,
};
use crate::model::{Command, PlaybackStatus};
use chrono::{DateTime, Local};
use gpui::{
    AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb, uniform_list,
};
use std::cell::RefCell;

// Re-measure only when content, playback labels, font, or UI scale changes.
// Layout growth belongs exclusively to Track, not these three content columns.
pub(super) struct HistoryColumnWidths {
    source: std::sync::Arc<Vec<crate::model::HistoryEntry>>,
    font: gpui::Font,
    rem_size: gpui::Pixels,
    playback: (Option<i64>, PlaybackStatus, Option<i64>),
    widths: [gpui::Pixels; 3],
}

impl History {
    pub(super) fn last_played_text(&self, track_id: i64, stored_at: Option<i64>) -> String {
        let current = self
            .state
            .current_track()
            .is_some_and(|track| track.id == track_id);
        if current
            && self.state.playback.status == PlaybackStatus::Playing
            && self.state.playback.last_heard_at.is_some()
        {
            return self.context.i18n.text("gl-played-now").into();
        }

        let played_at = if current && self.state.playback.status == PlaybackStatus::Paused {
            self.state.playback.last_heard_at
        } else {
            stored_at
        };
        match played_at.and_then(|time| DateTime::from_timestamp(time, 0)) {
            Some(time) => self.context.i18n.format(
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
            None => self.context.i18n.text("gl-played-none").into(),
        }
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
        let active = self.model.sort() == sort;
        let id = match sort {
            HistorySort::Recent => ("history-sort-recent", panel_id),
            HistorySort::Title => ("history-sort-title", panel_id),
            HistorySort::Plays => ("history-sort-plays", panel_id),
        };
        sortable_header(
            id,
            label,
            active,
            self.model.descending(),
            width,
            cx,
            move |this, _, cx| {
                this.model.set_sort(sort);
                cx.notify();
            },
        )
        .px_2()
        .py_1()
        .rounded_sm()
    }

    pub(in crate::gui) fn render_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        use crate::gui::list_interaction::{ContentDrag, ContentMenuRequest, TrackSource};

        let history_len = self.model.len();
        let history_total = self.model.total();
        self.search.update(cx, |search, cx| {
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
        let widths = if let Some(cache) = &self.column_widths
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
            let arrow = measure("󰁅".into(), gpui::font(crate::gui::fonts::SYMBOLS_FAMILY)).max(
                measure("󰁝".into(), gpui::font(crate::gui::fonts::SYMBOLS_FAMILY)),
            ) + rem_size * 0.25;
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
            self.column_widths = Some(HistoryColumnWidths {
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
        let scroll = self.context.scroll.borrow_mut().list(("history", panel_id));
        panel_surface(("history-panel", panel_id))
            .child(
                row()
                    .items_end()
                    .child(
                        list_panel_header(
                            theme,
                            self.tr("gl-recent-tracks"),
                            self.context.i18n
                                .format("gl-track-count", &[("count", history_total.into())]),
                        )
                        .flex_1()
                        .min_w(gpui::rems(3.)),
                    )
                    .child(self.search.clone()),
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
                                    let item = this.model.get(index)?;
                                    let id = item.track_id;
                                    let favorite = item.favorite;
                                    let title = if item.title.is_empty() {
                                        this.context.i18n.text("gl-untitled").to_owned()
                                    } else {
                                        item.title.clone()
                                    };
                                    let last_played =
                                        this.last_played_text(id, Some(item.played_at));
                                    let selected = this.model.contains(id);
                                    let generation = this.model.generation();
                                    let drag_owner = cx.weak_entity();
                                    let drag_i18n = this.context.i18n.clone();
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
                                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                                cx.stop_propagation();
                                                cx.emit(HistoryEvent::FocusRequested { panel_id });
                                                this.refresh(cx);
                                                let ids = this.model.drag_ids(id, generation);
                                                if ids.is_empty() {
                                                    return;
                                                }
                                                if !this.model.contains(id) {
                                                    this.request_selection(id, false, false, cx);
                                                }
                                                cx.emit(HistoryEvent::ContentMenu(RefCell::new(Some(ContentMenuRequest {
                                                    source: TrackSource::Tracks(ids),
                                                    position: event.position,
                                                    play: Some(Command::Play { track_id: id }),
                                                }))));
                                            }),
                                        )
                                        .on_click(cx.listener(
                                            move |this, event: &gpui::ClickEvent, _, cx| {
                                                cx.emit(HistoryEvent::FocusRequested { panel_id });
                                                let modifiers = event.modifiers();
                                                this.request_selection(
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
                                                    let ids = this.model.drag_ids(id, generation);
                                                    if !ids.is_empty() && !this.model.contains(id) {
                                                        this.request_selection(id, false, false, cx);
                                                    }
                                                    ids
                                                })
                                                .unwrap_or_default();
                                            let count = ids.len();
                                            drag.initialize(TrackSource::Tracks(ids));
                                            crate::gui::components::begin_list_drag(window, cx);
                                            let label = drag_i18n.format(
                                                "gl-track-count",
                                                &[("count", count.into())],
                                            ).into();
                                            cx.new(|_| crate::gui::components::ListDragPreview { label })
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
