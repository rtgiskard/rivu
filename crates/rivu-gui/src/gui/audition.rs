use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};

use anyhow::Result;
use futures::{Stream, StreamExt};
use gpui::{
    App, AppContext, Bounds, Context, InteractiveElement, IntoElement, Modifiers, MouseButton,
    ParentElement, Pixels, QuitMode, Render, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, canvas, div, prelude::FluentBuilder, px,
    rgb, size,
};
use rivu_presentation::i18n::Localizer;
use rivu_presentation::time::format_time;
use rivu_ui::{IconButton, IconButtonDensity, IconButtonStyle, Slider, SliderInput, SliderKey};

pub enum AuditionCommand {
    Pause(bool),
    Seek { seconds: f64, revision: u64 },
    Stop,
    Speed(f64),
}

pub enum AuditionEvent {
    Started(Option<f64>),
    Position { revision: u64, seconds: f64 },
    Ended,
    Failed(String),
}

/// Run a single-file player in the calling process, without attaching to Core.
pub fn run<S, C>(
    path: PathBuf,
    config: rivu_format::config::Config,
    source: Result<(S, C)>,
) -> Result<()>
where
    S: Stream<Item = AuditionEvent> + 'static,
    C: Fn(AuditionCommand) -> Result<bool> + 'static,
{
    let i18n = Localizer::new(&config.ui_language);
    let filename: SharedString = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
        .into();
    let (events, control, error) = match source {
        Ok((events, control)) => (Some(events), Some(Box::new(control) as Control), None),
        Err(error) => (None, None, Some(format!("{error:#}").into())),
    };
    let startup = Rc::new(RefCell::new(Ok(())));
    let result = startup.clone();
    gpui_platform::application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            let open = (|| -> Result<()> {
                crate::gui::init(cx)?;
                super::appearance::theme::set_theme(config.ui_theme, cx);
                let font = super::appearance::fonts::resolve(&config.ui_font, cx);
                super::appearance::fonts::publish(&font, config.ui_font_size_pt, cx);
                let bounds = Bounds::centered(None, size(px(384.), px(100.)), cx);
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(320.), px(100.))),
                        is_resizable: false,
                        titlebar: Some(TitlebarOptions {
                            title: Some("Rivu".into()),
                            ..Default::default()
                        }),
                        app_id: Some("rivu".into()),
                        ..Default::default()
                    },
                    move |window, cx| {
                        cx.new(|cx| {
                            Audition::new(filename, i18n, events, control, error, window, cx)
                        })
                    },
                )?;
                cx.on_window_closed(|cx, _| cx.quit()).detach();
                cx.activate(true);
                Ok(())
            })();
            if open.is_err() {
                *startup.borrow_mut() = open;
                cx.quit();
            }
        });
    result.replace(Ok(()))
}

type Control = Box<dyn Fn(AuditionCommand) -> Result<bool>>;

struct Audition {
    filename: SharedString,
    play_label: SharedString,
    pause_label: SharedString,
    seek_label: SharedString,
    speed_label: SharedString,
    control: Option<Control>,
    error: Option<SharedString>,
    control_error: Option<SharedString>,
    ready: bool,
    paused: bool,
    duration: Option<f64>,
    position: f64,
    revision: u64,
    speed: f64,
    speed_hold: Option<gpui::Task<()>>,
    preview: Option<f64>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    focus: gpui::FocusHandle,
}

impl Audition {
    fn new<S: Stream<Item = AuditionEvent> + 'static>(
        filename: SharedString,
        i18n: Localizer,
        events: Option<S>,
        control: Option<Control>,
        error: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let this = Self {
            filename,
            play_label: i18n.text("gl-play").to_owned().into(),
            pause_label: i18n.text("gl-pause").to_owned().into(),
            seek_label: i18n.text("gl-seek-position").to_owned().into(),
            speed_label: i18n.text("gl-playback-speed").to_owned().into(),
            control,
            error,
            control_error: None,
            ready: false,
            paused: false,
            duration: None,
            position: 0.,
            revision: 0,
            speed: 1.0,
            speed_hold: None,
            preview: None,
            bounds: Rc::default(),
            focus,
        };
        if let Some(events) = events {
            cx.spawn_in(window, async move |this, cx| {
                futures::pin_mut!(events);
                loop {
                    let event = events.next().await;
                    if !this
                        .update_in(cx, |this, window, cx| this.event(event, window, cx))
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() && this.preview.take().is_some() {
                cx.notify();
            }
        })
        .detach();
        this
    }

    fn event(
        &mut self,
        event: Option<AuditionEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match event {
            Some(AuditionEvent::Started(duration)) => {
                self.duration = duration.filter(|value| value.is_finite() && *value > 0.);
                self.ready = true;
            }
            Some(AuditionEvent::Position { revision, seconds }) if revision == self.revision => {
                self.position = seconds;
            }
            Some(AuditionEvent::Ended) => {
                window.remove_window();
                return false;
            }
            Some(AuditionEvent::Failed(message)) => self.error = Some(message.into()),
            None => self.error = Some("Audio worker stopped before playback completed".into()),
            _ => return true,
        }
        if self.error.is_some() {
            self.preview = None;
            if let Some(control) = &self.control {
                let _ = control(AuditionCommand::Stop);
            }
        }
        cx.notify();
        self.error.is_none()
    }

    fn send(&mut self, command: AuditionCommand) -> bool {
        let Some(control) = &self.control else {
            return false;
        };
        match control(command) {
            Ok(accepted) => {
                self.control_error = None;
                accepted
            }
            Err(error) => {
                self.control_error = Some(error.to_string().into());
                false
            }
        }
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.ready && self.error.is_none() {
            let paused = !self.paused;
            if self.send(AuditionCommand::Pause(paused)) {
                self.paused = paused;
            }
            cx.notify();
        }
    }

    fn pointer_position(&self, pointer: gpui::Point<Pixels>) -> Option<f64> {
        Some(f64::from(Slider::value_at(self.bounds.get(), pointer)?) * self.duration?)
    }

    fn seek(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let revision = self.revision + 1;
        if self.send(AuditionCommand::Seek { seconds, revision }) {
            self.revision = revision;
            self.position = seconds;
        }
        cx.notify();
    }
}

impl Render for Audition {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = rivu_ui::theme::palette(cx);
        let position = self.preview.unwrap_or(self.position);
        let root = div()
            .id("audition")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap_2()
            .px_4()
            .py_2()
            .bg(rgb(palette.bg))
            .text_color(rgb(palette.text))
            .text_size(px(14.))
            .font_size_scale(rivu_ui::theme::font_size_scale(cx))
            .font(rivu_ui::theme::font(cx).clone())
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.start_window_move();
                cx.stop_propagation();
            })
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                let keys = super::shell::key_modifiers(event);
                if let Some(delta) =
                    rivu_presentation::keyboard::speed_delta(event.keystroke.key.as_str(), keys)
                {
                    let speed = rivu_format::playback_speed::adjust(this.speed, delta);
                    window.prevent_default();
                    cx.stop_propagation();
                    if this.ready && this.error.is_none() && speed != this.speed {
                        if this.send(AuditionCommand::Speed(speed)) {
                            this.speed = speed;
                            this.speed_hold = super::playback::hold_normal_speed(
                                speed,
                                |this| &mut this.speed_hold,
                                cx,
                            );
                        }
                        cx.notify();
                    }
                    return;
                }
                if modifiers != Modifiers::default() {
                    return;
                }
                if event.keystroke.key == "space" && !event.is_held {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.toggle(cx);
                } else if event.keystroke.key == "escape" && this.preview.take().is_some() {
                    cx.notify();
                }
            }))
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.preview.is_some() {
                    this.preview = if event.pressed_button == Some(MouseButton::Left) {
                        this.pointer_position(event.position)
                    } else {
                        None
                    };
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                    if this.preview.take().is_some() {
                        if let Some(seconds) = this.pointer_position(event.position) {
                            this.seek(seconds, cx);
                        } else {
                            cx.notify();
                        }
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(seconds) = this.preview.take() {
                        this.seek(seconds, cx);
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().truncate().child(
                            self.control_error
                                .as_ref()
                                .unwrap_or(&self.filename)
                                .clone(),
                        ),
                    )
                    .when(self.speed != 1.0 || self.speed_hold.is_some(), |row| {
                        row.child(
                            super::playback::speed_badge(palette, self.speed, cx)
                                .id("audition-speed")
                                .tooltip(rivu_ui::text_tooltip(self.speed_label.clone())),
                        )
                    }),
            );
        if let Some(error) = &self.error {
            return root.child(div().text_sm().overflow_hidden().child(error.clone()));
        }
        let playing = self.ready && !self.paused;
        let toggle = IconButton::new(
            "toggle",
            if playing { "󰏤" } else { "" },
            if playing {
                self.pause_label.clone()
            } else {
                self.play_label.clone()
            },
        )
        .enabled(self.ready)
        .build(
            IconButtonStyle::Quiet,
            IconButtonDensity::Regular,
            cx,
            |this, _, cx| this.toggle(cx),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let bounds = self.bounds.clone();
        let slider = Slider::new(
            "seek",
            self.duration
                .map_or(0., |duration| (position / duration) as f32),
            self.seek_label.clone(),
        )
        .disabled(self.duration.is_none())
        .dragging(self.preview.is_some())
        .render(cx, |this, input, _, cx| match input {
            SliderInput::PointerDown { position, .. } => {
                cx.stop_propagation();
                this.preview = this.pointer_position(position);
                cx.notify();
            }
            SliderInput::Key(key) => {
                this.preview = None;
                if let Some(duration) = this.duration {
                    let seconds = match key {
                        SliderKey::Decrease | SliderKey::Increase => {
                            let seconds = this.position + super::playback::seek_slider_delta(key);
                            if key == SliderKey::Decrease {
                                seconds.max(0.)
                            } else {
                                seconds.min(duration)
                            }
                        }
                        SliderKey::Start => 0.,
                        SliderKey::End => duration,
                    };
                    this.seek(seconds, cx);
                }
            }
        })
        .child(
            canvas(move |geometry, _, _| bounds.set(geometry), |_, (), _, _| {})
                .absolute()
                .inset_0(),
        );
        let time = match self.duration {
            Some(duration) => format!("{} / {}", format_time(position), format_time(duration)),
            None => format_time(position),
        };
        root.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(toggle.flex_shrink_0())
                .child(div().flex_1().min_w_0().child(slider))
                .child(div().flex_shrink_0().text_sm().child(time)),
        )
    }
}
