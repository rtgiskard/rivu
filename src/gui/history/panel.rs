//! History list rendering; all row closures target the History owner.

use super::{History, HistoryEvent, HistorySort};
use crate::gui::{appearance::fonts::tabular_numbers, components::search::set_search_counts};
use crate::model::{Command, PlaybackStatus};
use chrono::{DateTime, Local};
use gpui::{AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb};
use rivu_ui::{
    VirtualList, caption, column, hover_actions, list_panel_header, list_row, list_viewport,
    panel_surface, row, sortable_header,
};
use std::cell::RefCell;

// Static content is independent of playback. At most one row's time label is
// replaced, so two maxima preserve the exact width without retaining every row.
pub(super) struct HistoryColumnWidths {
    source: std::sync::Arc<Vec<crate::model::HistoryEntry>>,
    font: gpui::Font,
    rem_size: gpui::Pixels,
    font_size_scale: f32,
    times: TimeWidths,
    playback: Option<(i64, PlaybackStatus, Option<i64>)>,
    playback_text: Option<SharedString>,
    widths: [gpui::Pixels; 3],
}

struct TimeWidths {
    header: gpui::Pixels,
    widest: [(Option<i64>, gpui::Pixels); 2],
}

impl TimeWidths {
    fn new(header: gpui::Pixels) -> Self {
        Self {
            header,
            widest: [(None, px(0.)); 2],
        }
    }

    // Each track occurs once in the history source; ties must retain both rows.
    fn include(&mut self, track_id: i64, width: gpui::Pixels) {
        if width >= self.widest[0].1 {
            self.widest[1] = self.widest[0];
            self.widest[0] = (Some(track_id), width);
        } else if width > self.widest[1].1 {
            self.widest[1] = (Some(track_id), width);
        }
    }

    fn width(&self, replaced: Option<i64>, replacement: gpui::Pixels) -> gpui::Pixels {
        let index = usize::from(replaced.is_some() && self.widest[0].0 == replaced);
        self.header.max(self.widest[index].1).max(replacement)
    }
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
        self.format_played_at(
            played_at,
            current && self.state.playback.status == PlaybackStatus::Paused,
        )
    }

    fn format_played_at(&self, played_at: Option<i64>, paused: bool) -> String {
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
                    ("paused", usize::from(paused).into()),
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
        crate::gui::components::track::track_favorite_toggle(
            id,
            group,
            favorite,
            |key| self.tr(key),
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
        )
    }

    fn history_sort_button(
        &self,
        panel_id: u64,
        label: impl Into<SharedString>,
        sort: HistorySort,
        width: Option<gpui::Pixels>,
        window: &Window,
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
        .px(window.rem_size() * 0.5 - px(1.))
        .py_1()
        .rounded_sm()
    }

    pub(in crate::gui) fn render_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        use crate::gui::list_interaction::{ContentDrag, ContentMenuRequest, TrackSource};

        let history_len = self.model.len();
        let history_total = self.model.total();
        self.search.update(cx, |search, cx| {
            set_search_counts(search, history_len, history_total, &self.context.i18n, cx)
        });
        // Playing uses one fixed label regardless of the latest heard timestamp.
        // Stopped or not-yet-heard playback leaves the stored labels untouched.
        let playback =
            self.state
                .current_track()
                .and_then(|track| match self.state.playback.status {
                    PlaybackStatus::Playing if self.state.playback.last_heard_at.is_some() => {
                        Some((track.id, PlaybackStatus::Playing, None))
                    }
                    PlaybackStatus::Paused => Some((
                        track.id,
                        PlaybackStatus::Paused,
                        self.state.playback.last_heard_at,
                    )),
                    _ => None,
                });
        let source = &self.state.library.history;
        let rem_size = window.rem_size();
        let font_size_scale = rivu_ui::theme::font_size_scale(cx);
        let measure = |text: SharedString, font: gpui::Font, size_scale: f32| {
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
                .shape_line(text, rem_size * 0.75 * size_scale, &[run], None)
                .width()
                .ceil()
        };
        let text_width = |text: SharedString| measure(text, self.ui_font.clone(), font_size_scale);
        let numeric_width = |text: SharedString| {
            let mut font = self.ui_font.clone();
            font.features = tabular_numbers();
            measure(text, font, font_size_scale)
        };
        if !self.column_widths.as_ref().is_some_and(|cache| {
            std::sync::Arc::ptr_eq(&cache.source, source)
                && cache.font == self.ui_font
                && cache.rem_size == rem_size
                && cache.font_size_scale == font_size_scale
        }) {
            // Keep space for either sort direction so sorting does not move columns.
            let arrow = measure(
                "󰁅".into(),
                gpui::font(crate::gui::appearance::fonts::SYMBOLS_FAMILY),
                1.,
            )
            .max(measure(
                "󰁝".into(),
                gpui::font(crate::gui::appearance::fonts::SYMBOLS_FAMILY),
                1.,
            )) + rem_size * 0.25;
            let mut widths = [
                text_width("#".into()),
                text_width(self.tr("gl-play-count")) + arrow,
                text_width(self.tr("gl-last-played")) + arrow,
            ];
            let mut times = TimeWidths::new(widths[2]);
            for (index, item) in source.iter().enumerate() {
                widths[0] = widths[0].max(numeric_width((index + 1).to_string().into()));
                widths[1] = widths[1].max(numeric_width(item.play_count.to_string().into()));
                let last_played = self.format_played_at(Some(item.played_at), false);
                times.include(item.track_id, text_width(last_played.into()));
            }
            widths[2] = times.width(None, px(0.));
            self.column_widths = Some(HistoryColumnWidths {
                source: std::sync::Arc::clone(source),
                font: self.ui_font.clone(),
                rem_size,
                font_size_scale,
                times,
                playback: None,
                playback_text: None,
                widths,
            });
        }
        if self.column_widths.as_ref().unwrap().playback != playback {
            let label = playback
                .filter(|(id, _, _)| source.iter().any(|item| item.track_id == *id))
                .map(|(id, _, _)| {
                    let text: SharedString = self.last_played_text(id, None).into();
                    let width = text_width(text.clone());
                    (text, width)
                });
            let cache = self.column_widths.as_mut().unwrap();
            cache.widths[2] = cache.times.width(
                playback.map(|(id, _, _)| id),
                label.as_ref().map_or(px(0.), |(_, width)| *width),
            );
            cache.playback = playback;
            cache.playback_text = label.map(|(text, _)| text);
        }
        // Header control borders fit inside the same cell inset as row text.
        let widths = self
            .column_widths
            .as_ref()
            .unwrap()
            .widths
            .map(|width| width + rem_size);
        let header = row()
            .items_center()
            .gap_0()
            .px(rem_size * 0.5 + px(1.))
            .py_1()
            .h_auto()
            .min_h(gpui::rems(2.625))
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
                window,
                cx,
            ))
            .child(self.history_sort_button(
                panel_id,
                self.tr("gl-play-count"),
                HistorySort::Plays,
                Some(widths[1]),
                window,
                cx,
            ))
            .child(self.history_sort_button(
                panel_id,
                self.tr("gl-last-played"),
                HistorySort::Recent,
                Some(widths[2]),
                window,
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
                })))
            })
            .child(list_viewport(
                column().size_full().child(header).child(VirtualList::new(("history-rows", panel_id), history_len, &scroll, cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
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
                            let last_played = this.column_widths.as_ref()
                                .filter(|cache| cache.playback.is_some_and(|(track, _, _)| track == id))
                                .and_then(|cache| cache.playback_text.clone())
                                .unwrap_or_else(|| this.last_played_text(id, Some(item.played_at)).into());
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
                                .h(rivu_ui::single_line_row_height(window, cx))
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
                                    rivu_ui::begin_list_drag(window, cx);
                                    let label = drag_i18n.format(
                                        "gl-track-count",
                                        &[("count", count.into())],
                                    ).into();
                                    cx.new(|_| rivu_ui::ListDragPreview { label })
                                }),
                            )
                        })
                        .collect::<Vec<_>>()
                }))
                .h_full()
                .flex_1()
                .min_h_0()
                .w_full().scrollbar(("history-scrollbar", panel_id))),
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_column_excludes_only_the_replaced_row_and_preserves_ties() {
        let mut times = TimeWidths::new(px(10.));
        for (id, width) in [(1, 30.), (2, 20.), (3, 15.)] {
            times.include(id, px(width));
        }
        assert_eq!(times.width(None, px(0.)), px(30.));
        assert_eq!(times.width(Some(1), px(12.)), px(20.));
        assert_eq!(times.width(Some(2), px(12.)), px(30.));
        assert_eq!(times.width(Some(99), px(0.)), px(30.));
        assert_eq!(times.width(Some(1), px(40.)), px(40.));

        times.include(4, px(30.));
        assert_eq!(times.width(Some(1), px(12.)), px(30.));
        assert_eq!(times.width(Some(4), px(12.)), px(30.));
    }

    #[test]
    fn time_column_keeps_header_space_for_empty_or_single_row_history() {
        let mut times = TimeWidths::new(px(10.));
        assert_eq!(times.width(None, px(0.)), px(10.));
        times.include(1, px(30.));
        assert_eq!(times.width(Some(1), px(5.)), px(10.));
        assert_eq!(times.width(Some(1), px(15.)), px(15.));
        assert_eq!(times.width(None, px(0.)), px(30.));
    }
}
