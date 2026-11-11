//! Playback controls own their Core commands; the window owns pointer arbitration.
use super::{
    appearance::fonts::tabular_numbers,
    feature::FeatureContext,
    interaction::{Dragging, SeekCommit},
};
use crate::projection::GuiSnapshot;
use gpui::{AnyElement, Context, EventEmitter, Pixels, Point, Window, div, prelude::*, rems, rgb};
use rivu_presentation::time::format_time;
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
        queue_id: Option<u64>,
    },
    SliderKey {
        slider: Dragging,
        key: SliderKey,
        queue_id: Option<u64>,
    },
    Error(String),
}

fn slider_command(
    slider: Dragging,
    key: SliderKey,
    queue_id: Option<u64>,
    current_queue_id: Option<u64>,
    playback: &PlaybackState,
) -> Option<Command> {
    match slider {
        Dragging::Seek(_) => {
            // Match pointer seeking: a known duration and queue occurrence are required.
            let queue_id = queue_id?;
            if Some(queue_id) != current_queue_id {
                return None;
            }
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

/// Pointer volume is absolute: keep one command in flight and only its latest successor.
#[derive(Default)]
struct VolumeIntent {
    in_flight: Option<f32>,
    next: Option<f32>,
}

impl VolumeIntent {
    /// Returns the value to send now; later values wait as one replaceable target.
    fn request(&mut self, value: f32) -> Option<f32> {
        match self.in_flight {
            None => {
                self.in_flight = Some(value);
                Some(value)
            }
            Some(sent) => {
                self.next = (value != sent).then_some(value);
                None
            }
        }
    }

    /// Completion of the in-flight command releases the latest waiting target.
    fn finish(&mut self) -> Option<f32> {
        self.in_flight = self.next.take();
        self.in_flight
    }

    /// A relative key intent must not be overwritten by an older unsent pointer value.
    fn discard_next(&mut self) {
        self.next = None;
    }

    fn target(&self) -> Option<f32> {
        self.next.or(self.in_flight)
    }
}

pub(super) struct Playback {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    seek_preview: Option<f64>,
    dragging: Option<Dragging>,
    volume: VolumeIntent,
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
            volume: VolumeIntent::default(),
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
        if self.displayed_volume() == value {
            return;
        }
        if let Some(value) = self.volume.request(value) {
            self.dispatch_volume(value, cx);
        }
        cx.notify();
    }

    fn displayed_volume(&self) -> f32 {
        self.volume.target().unwrap_or(self.state.playback.volume)
    }

    fn dispatch_volume(&mut self, value: f32, cx: &mut Context<Self>) {
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            let delivery = handle.request_ack(Command::Volume { value }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = delivery.observed_result() {
                    cx.emit(PlaybackEvent::Error(error));
                }
                if let Some(next) = this.volume.finish() {
                    this.dispatch_volume(next, cx);
                }
                this.reconcile(this.context.snapshot(), cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn slider_key(
        &mut self,
        slider: Dragging,
        key: SliderKey,
        queue_id: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.reconcile(self.context.snapshot(), cx);
        if matches!(slider, Dragging::Volume(_)) {
            self.volume.discard_next();
        }
        if let Some(command) = slider_command(
            slider,
            key,
            queue_id,
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
        let queue_id = self.state.queue.current_id;
        let seek = Slider::new(
            ("seek", panel_id),
            progress,
            self.context.tr("gl-seek-position"),
        )
        .disabled(!seek_enabled)
        .dragging(matches!(self.dragging, Some(Dragging::Seek(id)) if id == panel_id))
        .render(cx, move |_, input, _, cx| {
            cx.emit(Self::slider_event(
                Dragging::Seek(panel_id),
                input,
                queue_id,
            ));
        })
        .child(seek_measurement);
        let volume_focus = window
            .use_keyed_state(("volume-focus", panel_id), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let volume_dragging = matches!(self.dragging, Some(Dragging::Volume(id)) if id == panel_id);
        let displayed_volume = self.displayed_volume();
        let show_volume_value = volume_dragging
            || (volume_focus.is_focused(window) && window.last_input_was_keyboard());
        let volume = Slider::new(
            ("volume", panel_id),
            displayed_volume,
            self.context.tr("gl-volume-level"),
        )
        .dragging(volume_dragging)
        .render(cx, move |_, input, _, cx| {
            cx.emit(Self::slider_event(Dragging::Volume(panel_id), input, None));
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
                            .child(format!("{:.0}%", displayed_volume * 100.)),
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
                    displayed_volume * 100.
                )))
            });
        let scroll = self
            .context
            .scroll
            .borrow_mut()
            .content(("transport", panel_id));
        let icon_scale = rivu_ui::theme::font_size_scale(cx);
        let missing_metadata = |view: gpui::Stateful<gpui::Div>| {
            view.size(gpui::rems(icon_scale))
                .flex()
                .items_center()
                .justify_center()
                .text_size(gpui::rems(1.))
                .font_size_scale(icon_scale)
                .line_height(gpui::relative(1.))
                .text_color(rgb(theme.muted).alpha(0.6))
        };
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
                                            .focus_visible(|style| {
                                                style.text_color(rgb(theme.text))
                                            })
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
                                        Command::ToggleFavorite {
                                            track_ids: vec![track_id],
                                        },
                                        cx,
                                    );
                                },
                                |control| {
                                    control
                                        .p_0()
                                        .min_w_0()
                                        .min_h_0()
                                        .focus_visible(|style| {
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
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .line_height(gpui::relative(1.25))
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
                                .text_sm()
                                .when(!artist.trim().is_empty(), |view| {
                                    view.text_color(rgb(theme.text))
                                })
                                .when(artist.trim().is_empty(), missing_metadata),
                            )
                            .child(caption(theme, "·").text_sm().flex_shrink_0())
                            .child(
                                super::components::track::metadata_text(
                                    theme,
                                    ("playback-album", panel_id).into(),
                                    &album,
                                    "󰀥",
                                    self.context.i18n.text("gl-album-missing"),
                                )
                                .text_sm()
                                .when(!album.trim().is_empty(), |view| {
                                    view.text_color(rgb(theme.secondary))
                                })
                                .when(album.trim().is_empty(), missing_metadata),
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

    fn slider_event(
        dragging: Dragging,
        input: SliderInput,
        queue_id: Option<u64>,
    ) -> PlaybackEvent {
        match input {
            SliderInput::PointerDown { position, alt } => PlaybackEvent::SliderDown {
                dragging,
                position,
                alt,
                queue_id,
            },
            SliderInput::Key(key) => PlaybackEvent::SliderKey {
                slider: dragging,
                key,
                queue_id,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_volume_keeps_one_command_in_flight_and_delivers_the_latest_target() {
        let mut intent = VolumeIntent::default();
        assert_eq!(intent.request(0.2), Some(0.2));
        for value in [0.3, 0.4, 0.5] {
            assert_eq!(intent.request(value), None);
        }
        assert_eq!(intent.target(), Some(0.5));
        // Only the latest waiting value follows the acknowledged command.
        assert_eq!(intent.finish(), Some(0.5));
        assert_eq!(intent.finish(), None);
        assert_eq!(intent.target(), None);

        // Returning to the in-flight value leaves nothing extra to send.
        assert_eq!(intent.request(0.6), Some(0.6));
        assert_eq!(intent.request(0.7), None);
        assert_eq!(intent.request(0.6), None);
        assert_eq!(intent.finish(), None);

        // A key intent supersedes an unsent pointer value, not the active command.
        assert_eq!(intent.request(0.1), Some(0.1));
        assert_eq!(intent.request(0.9), None);
        intent.discard_next();
        assert_eq!(intent.target(), Some(0.1));
        assert_eq!(intent.finish(), None);
    }

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
            assert!(slider_command(Dragging::Seek(3), key, None, Some(10), &state).is_none());
            for duration in [
                None,
                Some(0.),
                Some(-1.),
                Some(f64::INFINITY),
                Some(f64::NAN),
            ] {
                state.duration = duration;
                assert!(
                    slider_command(Dragging::Seek(3), key, Some(10), Some(10), &state).is_none()
                );
            }
            state.duration = Some(42.);
        }
    }

    #[test]
    fn seek_keys_never_retarget_an_old_render_to_a_new_queue_occurrence() {
        let state = PlaybackState {
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
            let PlaybackEvent::SliderKey {
                slider,
                key,
                queue_id,
            } = Playback::slider_event(Dragging::Seek(3), SliderInput::Key(key), Some(10))
            else {
                panic!("seek key event");
            };
            // Occurrences, not track IDs: a repeated copy of the same song is new.
            assert!(slider_command(slider, key, queue_id, Some(11), &state).is_none());
            assert!(slider_command(slider, key, queue_id, None, &state).is_none());
            let command = slider_command(slider, key, queue_id, Some(10), &state).unwrap();
            match command {
                Command::SeekQueue { queue_id, seconds } => {
                    assert_eq!(queue_id, 10);
                    assert_eq!(seconds, if key == SliderKey::End { 42. } else { 0. });
                }
                Command::SeekRelative { queue_id, delta } => {
                    assert_eq!(queue_id, 10);
                    assert_eq!(delta, if key == SliderKey::Increase { 5. } else { -5. });
                }
                _ => panic!("seek command"),
            }
        }
    }

    #[test]
    fn volume_keys_remain_available_without_a_playable_format_or_queue() {
        let state = PlaybackState {
            duration: None,
            volume: 0.5,
            ..Default::default()
        };
        for (key, expected) in [
            (SliderKey::Start, Command::Volume { value: 0. }),
            (SliderKey::End, Command::Volume { value: 1. }),
            (SliderKey::Decrease, Command::AdjustVolume { delta: -0.05 }),
            (SliderKey::Increase, Command::AdjustVolume { delta: 0.05 }),
        ] {
            let actual = slider_command(Dragging::Volume(3), key, None, None, &state).unwrap();
            match (actual, expected) {
                (Command::Volume { value }, Command::Volume { value: expected }) => {
                    assert_eq!(value, expected);
                }
                (Command::AdjustVolume { delta }, Command::AdjustVolume { delta: expected }) => {
                    assert_eq!(delta, expected);
                }
                _ => panic!("volume command"),
            }
        }
    }
}
