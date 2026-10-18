//! Playback controls own their Core commands; the window owns pointer arbitration.
use super::{
    appearance::fonts::tabular_numbers,
    feature::FeatureContext,
    format_time,
    layout::interaction::{Dragging, SeekCommit},
};
use crate::{
    model::{Command, PlaybackStatus, RepeatMode},
    projection::GuiSnapshot,
};
use gpui::{
    AnyElement, Context, ElementId, EventEmitter, MouseButton, MouseDownEvent, Pixels, Point,
    Window, div, prelude::*, px, relative, rgb,
};
use rivu_ui::{
    active_control, caption, column, disabled_control, icon_button, quiet_icon_button, row,
};
use std::rc::Rc;

pub(super) enum PlaybackEvent {
    LocateQueue,
    SliderDown {
        dragging: Dragging,
        position: Point<Pixels>,
        alt: bool,
    },
    Error(String),
}

pub(super) struct Playback {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    seek_preview: Option<f64>,
    dragging: Option<Dragging>,
}

impl EventEmitter<PlaybackEvent> for Playback {}

impl Playback {
    pub(super) fn new(context: FeatureContext, _cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        Self {
            context,
            state,
            seek_preview: None,
            dragging: None,
        }
    }

    pub(super) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        if Rc::ptr_eq(&self.state, &state) {
            return;
        }
        let track_changed = match (self.state.current_track(), state.current_track()) {
            (Some(old), Some(new)) => {
                old.title != new.title
                    || old.artist != new.artist
                    || old.album != new.album
                    || old.duration != new.duration
            }
            (None, None) => false,
            _ => true,
        };
        let changed = self.state.playback != state.playback
            || self.state.queue.current_id != state.queue.current_id
            || track_changed;
        self.state = state;
        if changed {
            cx.notify();
        }
    }

    pub(super) fn set_interaction(
        &mut self,
        seek_preview: Option<f64>,
        dragging: Option<Dragging>,
        cx: &mut Context<Self>,
    ) {
        // Split drags have no playback visual state. Keep their lifecycle in the window.
        let dragging = dragging.filter(|dragging| !matches!(dragging, Dragging::Split(_)));
        let same_drag = match (self.dragging, dragging) {
            (None, None) => true,
            (Some(Dragging::Seek(old)), Some(Dragging::Seek(new)))
            | (Some(Dragging::Volume(old)), Some(Dragging::Volume(new))) => old == new,
            _ => false,
        };
        if self.seek_preview == seek_preview && same_drag {
            return;
        }
        self.seek_preview = seek_preview;
        self.dragging = dragging;
        cx.notify();
    }

    pub(super) fn commit_seek(&mut self, seek: SeekCommit, cx: &mut Context<Self>) {
        self.send(
            Command::SeekQueue {
                queue_id: seek.queue_id,
                seconds: seek.seconds,
            },
            cx,
        );
    }

    pub(super) fn set_volume(&mut self, value: f32, cx: &mut Context<Self>) {
        self.send(Command::Volume { value }, cx);
    }

    pub(super) fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.context.handle().send(command) {
            cx.emit(PlaybackEvent::Error(format!("{error:#}")));
        }
        cx.notify();
    }

    pub(super) fn render_panel(
        &mut self,
        panel_id: u64,
        seek_measurement: AnyElement,
        volume_measurement: AnyElement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let track = self.state.current_track();
        let title = track
            .map(|track| track.title.clone())
            .unwrap_or_else(|| self.context.i18n.text("gl-choose-track").into());
        let artist = track.map(|track| track.artist.clone()).unwrap_or_default();
        let album = track.map(|track| track.album.clone()).unwrap_or_default();
        let has_artist = !artist.trim().is_empty();
        let has_album = !album.trim().is_empty();
        let status = match self.state.playback.status {
            PlaybackStatus::Playing => self.context.tr("gl-playing"),
            PlaybackStatus::Paused => self.context.tr("gl-paused"),
            PlaybackStatus::Stopped => self.context.tr("gl-stopped"),
        };
        let duration = self
            .state
            .playback
            .duration
            .or_else(|| track.and_then(|track| track.duration));
        let preview_position = self.seek_preview.unwrap_or(self.state.playback.position);
        let progress = duration
            .filter(|duration| *duration > 0.)
            .map_or(0., |duration| (preview_position / duration) as f32);
        let seek = self.slider(
            ("seek", panel_id),
            progress,
            Dragging::Seek(panel_id),
            seek_measurement,
            cx,
        );
        let volume = self.slider(
            ("volume", panel_id),
            self.state.playback.volume,
            Dragging::Volume(panel_id),
            volume_measurement,
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
                            self.context.tr("gl-shuffle-on")
                        } else {
                            self.context.tr("gl-shuffle-off")
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
                        self.context.tr("gl-previous-track"),
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
                            self.context.tr("gl-pause")
                        } else {
                            self.context.tr("gl-play")
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
                        self.context.tr("gl-next-track"),
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
                            RepeatMode::Off => self.context.tr("gl-repeat-off"),
                            RepeatMode::All => self.context.tr("gl-repeat-all"),
                            RepeatMode::One => self.context.tr("gl-repeat-one"),
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
                    .font_family(super::appearance::fonts::SYMBOLS_FAMILY)
                    .child("󰕾")
                    .tooltip({
                        let text = self.context.tr("gl-volume");
                        rivu_ui::text_tooltip(text)
                    }),
            )
            .child(div().flex_1().min_w_0().child(volume));
        let scroll = self
            .context
            .scroll
            .borrow_mut()
            .content(("transport", panel_id));
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
                                        self.context.tr("gl-locate-playing-entry"),
                                        cx,
                                        |this, _, cx| {
                                            if this.state.queue.current_id.is_some() {
                                                cx.emit(PlaybackEvent::LocateQueue);
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

    fn slider(
        &self,
        id: impl Into<ElementId>,
        value: f32,
        dragging: Dragging,
        measurement: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let value = value.clamp(0., 1.);
        let is_dragging = match (self.dragging, dragging) {
            (Some(Dragging::Seek(active)), Dragging::Seek(id))
            | (Some(Dragging::Volume(active)), Dragging::Volume(id)) => active == id,
            _ => false,
        };
        div()
            .id(id)
            .group("playback-slider")
            .relative()
            .h(px(20.))
            .w_full()
            .min_w(px(60.))
            .cursor_pointer()
            .flex()
            .items_center()
            .child(
                div()
                    .w_full()
                    .h(px(4.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(theme.border))
                    .child(
                        div()
                            .h_full()
                            .w(relative(value))
                            .rounded_full()
                            .bg(rgb(theme.accent)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .mx(px(5.))
                    .flex()
                    .items_center()
                    .opacity(0.)
                    .group_hover("playback-slider", |style| style.opacity(1.))
                    .when(is_dragging, |view| view.opacity(1.))
                    .child(
                        div()
                            .relative()
                            .left(relative(value))
                            .ml(px(-5.))
                            .size(px(10.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(rgb(theme.text)),
                    ),
            )
            .child(measurement)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                    cx.emit(PlaybackEvent::SliderDown {
                        dragging,
                        position: event.position,
                        alt: event.modifiers.alt,
                    });
                }),
            )
            .when(is_dragging, |view| {
                // Volume release has no command; repaint after the root clears the drag.
                view.on_mouse_up(MouseButton::Left, cx.listener(|_, _, _, cx| cx.notify()))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(|_, _, _, cx| cx.notify()))
            })
            .into_any_element()
    }
}
