//! Playback controls own their Core commands; the window owns pointer arbitration.
use super::{
    appearance::fonts::tabular_numbers,
    feature::FeatureContext,
    format_time,
    layout::interaction::{Dragging, SeekCommit},
};
use crate::projection::GuiSnapshot;
use gpui::{AnyElement, Context, EventEmitter, Pixels, Point, Window, div, prelude::*, rems, rgb};
use rivu_protocol::model::{Command, PlaybackState, PlaybackStatus, RepeatMode};
use rivu_ui::{
    IconButton, IconButtonDensity, IconButtonGroup, IconButtonStyle, Slider, SliderInput,
    SliderKey, caption, column, row,
};
use std::rc::Rc;

pub(super) enum PlaybackEvent {
    LocateQueue,
    SliderDown {
        dragging: Dragging,
        position: Point<Pixels>,
        alt: bool,
    },
    SliderKey {
        slider: Dragging,
        key: SliderKey,
    },
    Error(String),
}

fn slider_command(
    slider: Dragging,
    key: SliderKey,
    queue_id: Option<u64>,
    playback: &PlaybackState,
) -> Option<Command> {
    match slider {
        Dragging::Seek(_) => {
            // Match pointer seeking: a known duration and queue occurrence are required.
            let queue_id = queue_id?;
            let duration = playback
                .duration
                .filter(|value| value.is_finite() && *value > 0.)?;
            Some(match key {
                SliderKey::Start => Command::SeekQueue {
                    queue_id,
                    seconds: 0.,
                },
                SliderKey::End => Command::SeekQueue {
                    queue_id,
                    seconds: duration,
                },
                SliderKey::Decrease | SliderKey::Increase => Command::SeekRelative {
                    queue_id,
                    delta: if key == SliderKey::Increase { 5. } else { -5. },
                },
            })
        }
        Dragging::Volume(_) => Some(match key {
            SliderKey::Start => Command::Volume { value: 0. },
            SliderKey::End => Command::Volume { value: 1. },
            SliderKey::Decrease | SliderKey::Increase => Command::AdjustVolume {
                delta: if key == SliderKey::Increase {
                    0.05
                } else {
                    -0.05
                },
            },
        }),
        Dragging::Split(_) => None,
    }
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
                old.id != new.id
                    || old.title != new.title
                    || old.artist != new.artist
                    || old.album != new.album
                    || old.duration != new.duration
                    || old.favorite != new.favorite
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

    pub(super) fn slider_key(&mut self, slider: Dragging, key: SliderKey, cx: &mut Context<Self>) {
        if let Some(command) = slider_command(
            slider,
            key,
            self.state.queue.current_id,
            &self.state.playback,
        ) {
            self.send(command, cx);
        }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let track = self.state.current_track();
        let title = track
            .map(|track| track.title.clone())
            .unwrap_or_else(|| self.context.i18n.text("gl-choose-track").into());
        let artist = track.map(|track| track.artist.clone()).unwrap_or_default();
        let album = track.map(|track| track.album.clone()).unwrap_or_default();
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
            .filter(|duration| duration.is_finite() && *duration > 0.)
            .map_or(0., |duration| (preview_position / duration) as f32);
        let seek_enabled = self.state.queue.current_id.is_some()
            && self
                .state
                .playback
                .duration
                .is_some_and(|value| value.is_finite() && value > 0.);
        let seek = Slider::new(
            ("seek", panel_id),
            progress,
            self.context.tr("gl-seek-position"),
        )
        .disabled(!seek_enabled)
        .dragging(matches!(self.dragging, Some(Dragging::Seek(id)) if id == panel_id))
        .render(cx, move |_, input, _, cx| {
            cx.emit(Self::slider_event(Dragging::Seek(panel_id), input));
        })
        .child(seek_measurement);
        let volume_focus = window
            .use_keyed_state(("volume-focus", panel_id), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let volume_dragging = matches!(self.dragging, Some(Dragging::Volume(id)) if id == panel_id);
        let show_volume_value = volume_dragging
            || (volume_focus.is_focused(window) && window.last_input_was_keyboard());
        let volume = Slider::new(
            ("volume", panel_id),
            self.state.playback.volume,
            self.context.tr("gl-volume-level"),
        )
        .dragging(volume_dragging)
        .render(cx, move |_, input, _, cx| {
            cx.emit(Self::slider_event(Dragging::Volume(panel_id), input));
        })
        .track_focus(&volume_focus)
        .child(volume_measurement)
        .when(show_volume_value, |view| {
            view.child(gpui::deferred(
                gpui::anchored()
                    .anchor(gpui::Anchor::BottomLeft)
                    .offset(gpui::point(gpui::px(0.), gpui::px(-4.)))
                    .snap_to_window_with_margin(gpui::px(4.))
                    .child(
                        rivu_ui::tooltip_surface(window, cx, 8.)
                            .whitespace_nowrap()
                            .child(format!("{:.0}%", self.state.playback.volume * 100.)),
                    ),
            ))
        });
        let controls = row()
            .gap_2()
            .flex_shrink_0()
            .justify_center()
            .child(
                IconButton::new(
                    ("shuffle", panel_id),
                    "󰒟",
                    self.context.tr(if self.state.playback.shuffle {
                        "gl-shuffle-on"
                    } else {
                        "gl-shuffle-off"
                    }),
                )
                .toggled(self.state.playback.shuffle)
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| this.send(Command::ToggleShuffle, cx),
                )
                .aspect_square()
                .rounded_full(),
            )
            .child(
                IconButtonGroup::new(IconButtonStyle::Quiet, IconButtonDensity::Regular)
                    .button_with(
                        IconButton::new(
                            ("previous", panel_id),
                            "󰒮",
                            self.context.tr("gl-previous-track"),
                        ),
                        cx,
                        |this, _, cx| this.send(Command::Previous, cx),
                        |button| {
                            button
                                .min_w(rems(2.))
                                .min_h(rems(2.))
                                .text_lg()
                                .aspect_square()
                                .rounded_full()
                        },
                    )
                    .button_with(
                        IconButton::new(
                            ("toggle", panel_id),
                            if self.state.playback.status == PlaybackStatus::Playing {
                                "󰏤"
                            } else {
                                ""
                            },
                            self.context.tr(
                                if self.state.playback.status == PlaybackStatus::Playing {
                                    "gl-pause"
                                } else {
                                    "gl-play"
                                },
                            ),
                        ),
                        cx,
                        |this, _, cx| this.send(Command::Toggle, cx),
                        |button| {
                            button
                                .min_w(rems(2.5))
                                .min_h(rems(2.5))
                                .text_xl()
                                .bg(rgb(theme.accent))
                                .text_color(rgb(theme.on_accent))
                                .group_hover("control", |style| style.bg(rgb(theme.accent_hover)))
                                .focus(|style| style.border_color(rgb(theme.text)))
                                .aspect_square()
                                .rounded_full()
                        },
                    )
                    .button_with(
                        IconButton::new(("next", panel_id), "󰒭", self.context.tr("gl-next-track")),
                        cx,
                        |this, _, cx| this.send(Command::Next, cx),
                        |button| {
                            button
                                .min_w(rems(2.))
                                .min_h(rems(2.))
                                .text_lg()
                                .aspect_square()
                                .rounded_full()
                        },
                    )
                    .into_div(),
            )
            .child(
                IconButton::new(
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
                )
                .selected(self.state.playback.repeat != RepeatMode::Off)
                .build_with(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Regular,
                    cx,
                    |this, _, cx| this.send(Command::CycleRepeat, cx),
                    |button| {
                        button.aspect_square().rounded_full().aria_value(
                            match self.state.playback.repeat {
                                RepeatMode::Off => self.context.tr("gl-repeat-off"),
                                RepeatMode::All => self.context.tr("gl-repeat-all"),
                                RepeatMode::One => self.context.tr("gl-repeat-one"),
                            },
                        )
                    },
                ),
            );
        let volume_control = row()
            .id(("volume-control", panel_id))
            .gap_1()
            .w(gpui::rems(9.))
            .max_w(gpui::relative(1.))
            .flex_shrink_0()
            .child(
                div()
                    .id(("volume-label", panel_id))
                    .flex_shrink_0()
                    .text_lg()
                    .text_color(rgb(theme.muted))
                    .font_family(super::appearance::fonts::SYMBOLS_FAMILY)
                    .font_size_scale(1.)
                    .child("󰕾"),
            )
            .child(div().flex_1().min_w_0().child(volume))
            .when(!show_volume_value, |control| {
                control.tooltip(rivu_ui::text_tooltip(format!(
                    "{:.0}%",
                    self.state.playback.volume * 100.
                )))
            });
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
            .gap_1()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .child(rivu_ui::panel_heading_row(
                div()
                    .min_w_0()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .truncate()
                    .child(title),
                rivu_ui::panel_header_actions()
                    .child(
                        IconButton::new(
                            ("playback-locate", panel_id),
                            "",
                            self.context.tr("gl-locate-playing-entry"),
                        )
                        .enabled(self.state.queue.current_id.is_some())
                        .build_with(
                            IconButtonStyle::Bare,
                            IconButtonDensity::Compact,
                            cx,
                            |this, _, cx| {
                                if this.state.queue.current_id.is_some() {
                                    cx.emit(PlaybackEvent::LocateQueue);
                                }
                            },
                            |control| {
                                control
                                    .p_0()
                                    .min_w_0()
                                    .min_h_0()
                                    .text_color(rgb(theme.muted).alpha(0.4))
                                    .when(self.state.queue.current_id.is_some(), |control| {
                                        control
                                            .group_hover("control", |style| {
                                                style
                                                    .bg(gpui::transparent_black())
                                                    .text_color(rgb(theme.text))
                                            })
                                            .focus(|style| style.text_color(rgb(theme.text)))
                                    })
                            },
                        ),
                    )
                    .child(caption(theme, status)),
            ))
            .child(
                row()
                    .gap_1()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .when_some(track, |view, track| {
                        let track_id = track.id;
                        let favorite = track.favorite;
                        view.child(
                            super::components::track::favorite_button(
                                ("playback-favorite", panel_id),
                                favorite,
                                |key| self.context.tr(key),
                            )
                            .build_with(
                                IconButtonStyle::Bare,
                                IconButtonDensity::Compact,
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
                                |control| {
                                    control
                                        .p_0()
                                        .min_w_0()
                                        .min_h_0()
                                        .focus(|style| {
                                            style.text_color(rgb(if favorite {
                                                theme.accent
                                            } else {
                                                theme.secondary
                                            }))
                                        })
                                        .group_hover("control", |style| {
                                            style.text_color(rgb(if favorite {
                                                theme.accent
                                            } else {
                                                theme.secondary
                                            }))
                                        })
                                },
                            ),
                        )
                    })
                    .child(
                        row()
                            .flex_1()
                            .min_w_0()
                            .child(
                                super::components::track::metadata_text(
                                    theme,
                                    ("playback-artist", panel_id).into(),
                                    &artist,
                                    "󰠃",
                                    self.context.i18n.text("gl-artist-missing"),
                                )
                                .when(!artist.trim().is_empty(), |view| {
                                    view.text_sm().text_color(rgb(theme.text))
                                })
                                .when(artist.trim().is_empty(), |view| {
                                    view.text_color(rgb(theme.muted).alpha(0.6))
                                }),
                            )
                            .child(caption(theme, "·").flex_shrink_0())
                            .child(
                                super::components::track::metadata_text(
                                    theme,
                                    ("playback-album", panel_id).into(),
                                    &album,
                                    "󰀥",
                                    self.context.i18n.text("gl-album-missing"),
                                )
                                .when(!album.trim().is_empty(), |view| {
                                    view.text_color(rgb(theme.secondary))
                                })
                                .when(album.trim().is_empty(), |view| {
                                    view.text_color(rgb(theme.muted).alpha(0.6))
                                }),
                            ),
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

    fn slider_event(dragging: Dragging, input: SliderInput) -> PlaybackEvent {
        match input {
            SliderInput::PointerDown { position, alt } => PlaybackEvent::SliderDown {
                dragging,
                position,
                alt,
            },
            SliderInput::Key(key) => PlaybackEvent::SliderKey {
                slider: dragging,
                key,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::layout::interaction::{DragCommit, LayoutInteraction};

    #[test]
    fn seek_keys_require_queue_identity_and_finite_positive_duration() {
        let mut state = PlaybackState {
            position: 12.,
            duration: Some(42.),
            ..Default::default()
        };
        for key in [
            SliderKey::Decrease,
            SliderKey::Increase,
            SliderKey::Start,
            SliderKey::End,
        ] {
            assert!(slider_command(Dragging::Seek(3), key, None, &state).is_none());
            for duration in [
                None,
                Some(0.),
                Some(-1.),
                Some(f64::INFINITY),
                Some(f64::NAN),
            ] {
                state.duration = duration;
                assert!(slider_command(Dragging::Seek(3), key, Some(10), &state).is_none());
            }
            state.duration = Some(42.);
        }
    }

    #[test]
    fn slider_key_cancellation_discards_pointer_preview_before_command() {
        let mut owner = LayoutInteraction::default();
        owner.begin_slider(Dragging::Seek(3), Some(10), Some(100.));
        owner.update_slider_seek(0.75);
        owner.cancel();
        let state = PlaybackState {
            position: 12.,
            duration: Some(100.),
            ..Default::default()
        };
        assert!(matches!(
            slider_command(Dragging::Seek(3), SliderKey::Increase, Some(10), &state),
            Some(Command::SeekRelative {
                queue_id: 10,
                delta: 5.
            })
        ));
        assert_eq!(owner.seek_preview(), None);
        assert_eq!(owner.finish_drag(), DragCommit::None);
    }
}
