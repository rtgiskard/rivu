use super::{
    ACCENT, BORDER, ButtonTooltip, DropdownItem, DropdownState, ERROR, GuiApp, HIGHLIGHT, Measured,
    PANEL, POPOVER_MAX_HEIGHT, UI_INSET, button, caption, column, copyable_message,
    dropdown_container, dropdown_row, icon_button, input::Input, panels::TRACK_HEIGHT, row,
};
use crate::{
    config::{Config, RgbColor, SpectrumStyle, SpectrumWindow, VisualizationPalette},
    model::{Command, RepeatMode},
};
use anyhow::{Context as _, Result};
use gpui::{prelude::*, *};
use std::{collections::HashMap, path::PathBuf, str::FromStr};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Roots,
    Volume,
    PlayCountThreshold,
    Font,
    Scale,
    Fps,
    Background,
    SpectrumMinHz,
    SpectrumMaxHz,
    SpectrumDb,
    SpectrumBandsPerOctave,
    SpectrumBarWidth,
    SpectrumBars,
    SpectrumGap,
    SpectrumHold,
    SpectrumGravity,
    SpectrumBarHold,
    SpectrumBarGravity,
    SpectrumSmoothing,
    SpectrogramMinHz,
    SpectrogramMaxHz,
    SpectrogramDb,
    SpectrogramHistory,
    CursorColor,
    Glow,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Roots => "Library roots (separate paths with semicolons)",
            Self::Volume => "Volume (0–100%)",
            Self::PlayCountThreshold => "Play count threshold (%)",
            Self::Font => "Interface font family (sans-serif, serif, monospace, or installed name)",
            Self::Scale => "Interface scale (0.75–2)",
            Self::Fps => "Analysis refresh rate (5–60 fps)",
            Self::Background => "Background (#RRGGBB)",
            Self::SpectrumMinHz | Self::SpectrogramMinHz => "Minimum frequency (Hz)",
            Self::SpectrumMaxHz | Self::SpectrogramMaxHz => "Maximum frequency (Hz)",
            Self::SpectrumDb | Self::SpectrogramDb => "Dynamic range (1–160 dB)",
            Self::SpectrumBandsPerOctave => "Bands per octave (1–48)",
            Self::SpectrumBarWidth => "Auto bar width (1–20 px)",
            Self::SpectrumBars => "Bar count (0 = auto; 1–512)",
            Self::SpectrumGap => "Bar gap (0–8 px)",
            Self::SpectrumHold => "Peak hold (0–2000 ms)",
            Self::SpectrumGravity => "Peak gravity (0–500 dB/s²)",
            Self::SpectrumBarHold => "Bar hold (0–2000 ms)",
            Self::SpectrumBarGravity => "Bar gravity (0–500 dB/s²)",
            Self::SpectrumSmoothing => "Release smoothing (0–1000 ms)",
            Self::SpectrogramHistory => "History (5–120 seconds)",
            Self::CursorColor => "Waterline color (#RRGGBB)",
            Self::Glow => "Glow strength (0–2; 0 = off)",
        }
    }
}

fn visual_switch(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    enabled: bool,
    cx: &mut Context<GuiApp>,
    toggle: impl Fn(&mut Config) + 'static,
) -> Stateful<Div> {
    icon_button(
        id,
        icon,
        format!("{label}: {}", if enabled { "On" } else { "Off" }),
        cx,
        move |this, _, cx| {
            toggle(&mut this.settings.draft);
            cx.notify();
        },
    )
    .size(rems(4.))
    .text_size(rems(1.75))
    .when(enabled, |view| {
        view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))
    })
}

fn visualization_palette_label(palette: VisualizationPalette) -> &'static str {
    match palette {
        VisualizationPalette::TokyoNight => "TokyoNight",
        VisualizationPalette::Deadbeef => "DeaDBeeF",
        VisualizationPalette::Nord => "Nord",
    }
}

fn next_visualization_palette(palette: VisualizationPalette) -> VisualizationPalette {
    match palette {
        VisualizationPalette::TokyoNight => VisualizationPalette::Deadbeef,
        VisualizationPalette::Deadbeef => VisualizationPalette::Nord,
        VisualizationPalette::Nord => VisualizationPalette::TokyoNight,
    }
}

fn spectrum_style_label(style: SpectrumStyle) -> &'static str {
    match style {
        SpectrumStyle::Bars => "Bars",
        SpectrumStyle::Outline => "Outline",
        SpectrumStyle::Led => "LED",
        SpectrumStyle::Line => "Line",
        SpectrumStyle::Solid => "Solid",
    }
}

fn ffmpeg_hint(status: &str) -> String {
    let mut text = String::with_capacity(status.len() + 24);
    text.push_str("FFmpeg audio decoding");
    if let Some(libraries) = status
        .strip_prefix("FFmpeg runtime available (")
        .and_then(|detail| detail.strip_suffix(')'))
    {
        for library in libraries
            .split(';')
            .next()
            .unwrap_or(libraries)
            .split(" / ")
        {
            text.push('\n');
            text.push_str(library);
        }
    } else {
        text.push('\n');
        text.push_str(status);
    }
    text
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    General,
    Visualizations,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualPage {
    Common,
    Spectrum,
    Spectrogram,
    Waveform,
}

/// Unsaved preferences stay independent of live playback snapshots.
pub(super) struct Settings {
    draft: Config,
    inputs: HashMap<Field, Entity<Input>>,
    device_dropdown: DropdownState<usize>,
    device_scroll: UniformListScrollHandle,
    applying: bool,
    session: u64,
    feedback: Option<std::result::Result<(), String>>,
    page: SettingsPage,
    visual_page: VisualPage,
}

impl Settings {
    pub(super) fn new(config: &Config, cx: &mut App) -> Self {
        let inputs = [
            Field::Roots,
            Field::Volume,
            Field::PlayCountThreshold,
            Field::Font,
            Field::Scale,
            Field::Fps,
            Field::Background,
            Field::SpectrumMinHz,
            Field::SpectrumMaxHz,
            Field::SpectrumDb,
            Field::SpectrumBandsPerOctave,
            Field::SpectrumBarWidth,
            Field::SpectrumBars,
            Field::SpectrumGap,
            Field::SpectrumHold,
            Field::SpectrumGravity,
            Field::SpectrumBarHold,
            Field::SpectrumBarGravity,
            Field::SpectrumSmoothing,
            Field::SpectrogramMinHz,
            Field::SpectrogramMaxHz,
            Field::SpectrogramDb,
            Field::SpectrogramHistory,
            Field::CursorColor,
            Field::Glow,
        ]
        .into_iter()
        .map(|field| (field, cx.new(|cx| Input::new("", field.label(), cx))))
        .collect();
        let mut settings = Self {
            draft: config.clone(),
            inputs,
            device_dropdown: DropdownState::default(),
            device_scroll: UniformListScrollHandle::new(),
            applying: false,
            session: 0,
            feedback: None,
            page: SettingsPage::General,
            visual_page: VisualPage::Common,
        };
        settings.fill(cx);
        settings
    }

    fn field(&self, field: Field) -> Div {
        column()
            .w_full()
            .flex_shrink_0()
            .gap_1()
            .child(caption(field.label()))
            .child(self.inputs[&field].clone())
    }

    fn pair(&self, first: Field, second: Field) -> Div {
        row()
            .items_start()
            .flex_wrap()
            .child(self.field(first).flex_1().min_w(px(180.)))
            .child(self.field(second).flex_1().min_w(px(180.)))
    }

    fn value(&self, field: Field, cx: &App) -> String {
        self.inputs[&field].read(cx).text().to_owned()
    }

    fn number<T: FromStr>(&self, field: Field, cx: &App) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        self.value(field, cx)
            .trim()
            .parse()
            .map_err(|error| anyhow::anyhow!("{}: {error}", field.label()))
    }

    fn set_value(&mut self, field: Field, value: impl Into<SharedString>, cx: &mut App) {
        self.inputs[&field].update(cx, |input, cx| input.set_text(value.into(), cx));
    }

    fn reset(&mut self, config: &Config, cx: &mut App) {
        self.draft = config.clone();
        self.applying = false;
        self.feedback = None;
        self.fill(cx);
    }

    fn fill(&mut self, cx: &mut App) {
        self.set_value(
            Field::Roots,
            self.draft
                .library_roots
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join(";"),
            cx,
        );
        self.set_value(Field::Volume, (self.draft.volume * 100.).to_string(), cx);
        self.set_value(
            Field::PlayCountThreshold,
            self.draft.play_count_threshold_percent.to_string(),
            cx,
        );
        self.set_value(Field::Font, self.draft.ui_font.clone(), cx);
        self.set_value(Field::Scale, self.draft.ui_scale.to_string(), cx);
        self.set_value(Field::Fps, self.draft.analysis_fps.to_string(), cx);
        self.set_value(
            Field::Background,
            self.draft.visual_background.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumMinHz,
            self.draft.spectrum_min_hz.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumMaxHz,
            self.draft.spectrum_max_hz.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumDb,
            self.draft.spectrum_db_range.to_string(),
            cx,
        );
        self.set_value(Field::SpectrumGap, self.draft.spectrum_gap.to_string(), cx);
        self.set_value(
            Field::SpectrumHold,
            self.draft.spectrum_peak_hold_ms.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumGravity,
            self.draft.spectrum_peak_gravity.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBarHold,
            self.draft.spectrum_bar_hold_ms.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBarGravity,
            self.draft.spectrum_bar_gravity.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumSmoothing,
            self.draft.spectrum_smoothing_ms.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBandsPerOctave,
            self.draft.spectrum_bands_per_octave.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBarWidth,
            self.draft.spectrum_bar_width.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrumBars,
            self.draft.spectrum_bars.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramMinHz,
            self.draft.spectrogram_min_hz.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramMaxHz,
            self.draft.spectrogram_max_hz.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramDb,
            self.draft.spectrogram_db_range.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramHistory,
            self.draft.spectrogram_history_seconds.to_string(),
            cx,
        );
        self.set_value(
            Field::CursorColor,
            self.draft.waveform_cursor_color.to_string(),
            cx,
        );
        self.set_value(Field::Glow, self.draft.waveform_glow.to_string(), cx);
    }

    fn parse(&self, cx: &App) -> Result<Config> {
        let mut config = self.draft.clone();
        config.library_roots = self
            .value(Field::Roots, cx)
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .collect();
        config.volume = self.number::<f32>(Field::Volume, cx)? / 100.;
        config.play_count_threshold_percent = self.number(Field::PlayCountThreshold, cx)?;
        config.ui_font = self.value(Field::Font, cx).trim().to_owned();
        config.ui_scale = self.number(Field::Scale, cx)?;
        config.analysis_fps = self.number(Field::Fps, cx)?;
        config.visual_background = self
            .value(Field::Background, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Visualization background")?;
        config.spectrum_min_hz = self.number(Field::SpectrumMinHz, cx)?;
        config.spectrum_max_hz = self.number(Field::SpectrumMaxHz, cx)?;
        config.spectrum_db_range = self.number(Field::SpectrumDb, cx)?;
        config.spectrum_gap = self.number(Field::SpectrumGap, cx)?;
        config.spectrum_peak_hold_ms = self.number(Field::SpectrumHold, cx)?;
        config.spectrum_peak_gravity = self.number(Field::SpectrumGravity, cx)?;
        config.spectrum_bar_hold_ms = self.number(Field::SpectrumBarHold, cx)?;
        config.spectrum_bar_gravity = self.number(Field::SpectrumBarGravity, cx)?;
        config.spectrum_smoothing_ms = self.number(Field::SpectrumSmoothing, cx)?;
        config.spectrum_bands_per_octave = self.number(Field::SpectrumBandsPerOctave, cx)?;
        config.spectrum_bar_width = self.number(Field::SpectrumBarWidth, cx)?;
        config.spectrum_bars = self.number(Field::SpectrumBars, cx)?;
        config.spectrogram_min_hz = self.number(Field::SpectrogramMinHz, cx)?;
        config.spectrogram_max_hz = self.number(Field::SpectrogramMaxHz, cx)?;
        config.spectrogram_db_range = self.number(Field::SpectrogramDb, cx)?;
        config.spectrogram_history_seconds = self.number(Field::SpectrogramHistory, cx)?;
        config.waveform_cursor_color = self
            .value(Field::CursorColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Waveform waterline color")?;
        config.waveform_glow = self.number(Field::Glow, cx)?;
        config.validate()?;
        Ok(config)
    }

    fn tab_button(
        &self,
        id: &'static str,
        label: &'static str,
        active: bool,
        cx: &mut Context<GuiApp>,
        select: impl Fn(&mut Settings) + 'static,
    ) -> Stateful<Div> {
        button(id, label, cx, move |this, _, cx| {
            this.settings.device_dropdown.close();
            select(&mut this.settings);
            cx.notify();
        })
        .flex_shrink_0()
        .when(active, |view| {
            view.bg(rgb(HIGHLIGHT))
                .text_color(rgb(ACCENT))
                .border_color(rgb(ACCENT))
        })
    }
}

impl GuiApp {
    pub(super) fn load_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.device_dropdown.close();
        let config = self.handle.state.read().config.as_ref().clone();
        self.settings.reset(&config, cx);
        cx.notify();
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.applying {
            return;
        }
        let config = match self.settings.parse(cx) {
            Ok(config) => config,
            Err(error) => {
                self.settings.feedback = Some(Err(format!("Invalid settings: {error:#}")));
                cx.notify();
                return;
            }
        };
        self.settings.applying = true;
        self.settings.feedback = None;
        let session = self.settings.session;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.request(Command::Configure { config }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.settings.session != session {
                    return;
                }
                this.settings.applying = false;
                this.settings.feedback = Some(if response.ok {
                    Ok(())
                } else {
                    Err(response
                        .error
                        .unwrap_or_else(|| "Could not save settings".into()))
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_device(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == 0 {
            self.settings.draft.output_device = None;
        } else if let Some(name) = self.state.devices.get(index - 1) {
            self.settings.draft.output_device = Some(name.clone());
        } else {
            return;
        }
        self.settings.device_dropdown.select_index(index);
        self.settings.device_dropdown.close();
        cx.notify();
    }

    pub(super) fn device_dropdown_is_open(&self) -> bool {
        self.settings.device_dropdown.is_open()
    }

    pub(super) fn close_device_dropdown(&mut self) {
        self.settings.device_dropdown.close();
    }

    pub(super) fn device_key(&mut self, key: &str, cx: &mut Context<Self>) {
        match key {
            "up" => self.settings.device_dropdown.move_previous(),
            "down" => self.settings.device_dropdown.move_next(),
            "enter" | "space" => {
                if let Some(index) = self.settings.device_dropdown.selected_index() {
                    self.choose_device(index, cx);
                }
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        if let Some(index) = self.settings.device_dropdown.selected_index() {
            self.settings
                .device_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn general_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let chosen_device: SharedString = self
            .settings
            .draft
            .output_device
            .clone()
            .unwrap_or_else(|| "System default".into())
            .into();
        let device = column()
            .flex_shrink_0()
            .gap_1()
            .child(caption("Output device"))
            .child(
                row()
                    .id("output-device-selector")
                    .relative()
                    .w_full()
                    .h(rems(2.))
                    .px(gpui::px(UI_INSET))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_sm()
                    .bg(rgb(PANEL))
                    .cursor_pointer()
                    .hover(|style| style.border_color(rgb(ACCENT)))
                    .child(div().flex_1().truncate().child(chosen_device.clone()))
                    .child("⌄")
                    .tooltip(move |_, cx| {
                        cx.new(|_| ButtonTooltip {
                            text: chosen_device.clone(),
                        })
                        .into()
                    })
                    .child(self.measurement(Measured::Device))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let open = this.settings.device_dropdown.is_open();
                        if open {
                            this.settings.device_dropdown.close();
                        } else {
                            let selected = this
                                .settings
                                .draft
                                .output_device
                                .as_ref()
                                .and_then(|name| {
                                    this.state.devices.iter().position(|device| device == name)
                                })
                                .map_or(0, |index| index + 1);
                            let items = std::iter::once(DropdownItem::new(0, "System default"))
                                .chain(this.state.devices.iter().enumerate().map(
                                    |(index, device)| DropdownItem::new(index + 1, device.clone()),
                                ));
                            this.settings.device_dropdown.open(items);
                            this.settings.device_dropdown.select_index(selected);
                        }
                        if let Some(index) = this.settings.device_dropdown.selected_index() {
                            this.settings
                                .device_scroll
                                .scroll_to_item(index, ScrollStrategy::Nearest);
                        }
                        this.settings_focus.focus(window, cx);
                        cx.notify();
                    })),
            );
        let draft = &self.settings.draft;
        let (shuffle, repeat, mpris, ffmpeg, remix) = (
            draft.shuffle,
            draft.repeat,
            draft.mpris_enabled,
            draft.ffmpeg_enabled,
            draft.pipewire_auto_mix,
        );
        let switches = row()
            .flex_shrink_0()
            .flex_wrap()
            .mt_3()
            .child(
                icon_button("settings-shuffle", "󰒟", "Shuffle", cx, |this, _, cx| {
                    this.settings.draft.shuffle = !this.settings.draft.shuffle;
                    cx.notify();
                })
                .size(rems(4.))
                .text_size(rems(1.75))
                .when(shuffle, |view| {
                    view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))
                }),
            )
            .child(
                icon_button(
                    "settings-repeat",
                    if repeat == RepeatMode::One {
                        "󰑘"
                    } else {
                        "󰑖"
                    },
                    "Repeat",
                    cx,
                    |this, _, cx| {
                        this.settings.draft.repeat = match this.settings.draft.repeat {
                            RepeatMode::Off => RepeatMode::All,
                            RepeatMode::All => RepeatMode::One,
                            RepeatMode::One => RepeatMode::Off,
                        };
                        cx.notify();
                    },
                )
                .size(rems(4.))
                .text_size(rems(1.75))
                .when(repeat != RepeatMode::Off, |view| {
                    view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))
                }),
            )
            .child(
                icon_button(
                    "settings-mpris",
                    "󰐹",
                    format!("Media controls (MPRIS)\n{}", self.state.mpris_status),
                    cx,
                    |this, _, cx| {
                        this.settings.draft.mpris_enabled = !this.settings.draft.mpris_enabled;
                        cx.notify();
                    },
                )
                .size(rems(4.))
                .text_size(rems(1.75))
                .when(mpris, |view| {
                    view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))
                }),
            )
            .child(
                icon_button(
                    "settings-ffmpeg",
                    "\u{f384}",
                    ffmpeg_hint(&self.state.ffmpeg_status),
                    cx,
                    |this, _, cx| {
                        this.settings.draft.ffmpeg_enabled = !this.settings.draft.ffmpeg_enabled;
                        cx.notify();
                    },
                )
                .font_family("Symbols Nerd Font")
                .size(rems(4.))
                .text_size(rems(1.75))
                .when(ffmpeg, |view| {
                    view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))
                }),
            )
            .child(
                icon_button(
                    "settings-pipewire-auto-mix",
                    "󱀞",
                    "PipeWire automatic channel remix",
                    cx,
                    |this, _, cx| {
                        this.settings.draft.pipewire_auto_mix =
                            !this.settings.draft.pipewire_auto_mix;
                        cx.notify();
                    },
                )
                .font_family("Symbols Nerd Font")
                .size(rems(4.))
                .text_size(rems(1.75))
                .when(remix, |view| {
                    view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))
                }),
            );
        let nerd_symbols = self.settings.draft.nerd_symbols;
        let switches = switches.child(
            icon_button(
                "settings-nerd-symbols",
                "Nf",
                "Nerd Symbols",
                cx,
                |this, _, cx| {
                    this.settings.draft.nerd_symbols = !this.settings.draft.nerd_symbols;
                    cx.notify();
                },
            )
            .size(rems(4.))
            .text_size(rems(1.75))
            .when(nerd_symbols, |view| {
                view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))
            }),
        );
        column()
            .gap_3()
            .child(self.settings.field(Field::Roots))
            .child(device)
            .child(self.settings.pair(Field::Volume, Field::PlayCountThreshold))
            .child(self.settings.field(Field::Font))
            .child(self.settings.field(Field::Scale))
            .child(switches)
    }

    fn visual_settings_tabs(&self, cx: &mut Context<Self>) -> Div {
        let selected = self.settings.visual_page;
        row()
            .flex_wrap()
            .flex_shrink_0()
            .gap_1()
            .px(gpui::px(UI_INSET))
            .py_2()
            .child(self.settings.tab_button(
                "visual-common",
                "Common",
                selected == VisualPage::Common,
                cx,
                |settings| settings.visual_page = VisualPage::Common,
            ))
            .child(self.settings.tab_button(
                "visual-spectrum",
                "Spectrum",
                selected == VisualPage::Spectrum,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrum,
            ))
            .child(self.settings.tab_button(
                "visual-spectrogram",
                "Spectrogram",
                selected == VisualPage::Spectrogram,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrogram,
            ))
            .child(self.settings.tab_button(
                "visual-waveform",
                "Waveform",
                selected == VisualPage::Waveform,
                cx,
                |settings| settings.visual_page = VisualPage::Waveform,
            ))
    }

    fn visual_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let settings = &self.settings;
        let draft = &settings.draft;
        match settings.visual_page {
            VisualPage::Common => column()
                .gap_3()
                .child(button(
                    "visual-palette",
                    format!(
                        "Visualization palette: {}  ›",
                        visualization_palette_label(draft.visual_palette)
                    ),
                    cx,
                    |this, _, cx| {
                        this.settings.draft.visual_palette =
                            next_visualization_palette(this.settings.draft.visual_palette);
                        cx.notify();
                    },
                ))
                .child(settings.pair(Field::Fps, Field::Background)),
            VisualPage::Spectrum => {
                let style = spectrum_style_label(draft.spectrum_style);
                column().gap_3()
                    .child(button("spectrum-style", format!("Style: {style}  ›"), cx, |this, _, cx| {
                        this.settings.draft.spectrum_style = match this.settings.draft.spectrum_style {
                            SpectrumStyle::Bars => SpectrumStyle::Outline,
                            SpectrumStyle::Outline => SpectrumStyle::Led,
                            SpectrumStyle::Led => SpectrumStyle::Line,
                            SpectrumStyle::Line => SpectrumStyle::Solid,
                            SpectrumStyle::Solid => SpectrumStyle::Bars,
                        };
                        cx.notify();
                    }))
                    .child(button("spectrum-fft", format!("FFT size: {}  ›", draft.spectrum_fft_size), cx, |this, _, cx| {
                        this.settings.draft.spectrum_fft_size = match this.settings.draft.spectrum_fft_size {
                            512 => 1024, 1024 => 2048, 2048 => 4096, 4096 => 8192,
                            8192 => 16384, 16384 => 32768, _ => 512,
                        };
                        cx.notify();
                    }))
                    .child(button("spectrum-window", format!("Window: {}  ›", match draft.spectrum_window {
                        SpectrumWindow::Hann => "Hann",
                        SpectrumWindow::BlackmanHarris => "Blackman–Harris",
                        SpectrumWindow::None => "None",
                    }), cx, |this, _, cx| {
                        this.settings.draft.spectrum_window = match this.settings.draft.spectrum_window {
                            SpectrumWindow::Hann => SpectrumWindow::BlackmanHarris,
                            SpectrumWindow::BlackmanHarris => SpectrumWindow::None,
                            SpectrumWindow::None => SpectrumWindow::Hann,
                        };
                        cx.notify();
                    }))
                    .child(settings.pair(Field::SpectrumMinHz, Field::SpectrumMaxHz))
                    .child(settings.pair(Field::SpectrumDb, Field::SpectrumBandsPerOctave))
                    .child(settings.pair(Field::SpectrumBarWidth, Field::SpectrumGap))
                    .child(settings.pair(Field::SpectrumBars, Field::SpectrumSmoothing))
                    .child(settings.pair(Field::SpectrumBarHold, Field::SpectrumBarGravity))
                    .child(settings.pair(Field::SpectrumHold, Field::SpectrumGravity))
                    .child(row().flex_wrap()
                        .child(visual_switch("spectrum-interpolate", "≈", "Interpolation", draft.spectrum_interpolate, cx,
                            |draft| draft.spectrum_interpolate = !draft.spectrum_interpolate))
                        .child(visual_switch("spectrum-scale", "ln", "Logarithmic frequency axis", draft.spectrum_log_scale, cx,
                            |draft| draft.spectrum_log_scale = !draft.spectrum_log_scale))
                        .child(visual_switch("spectrum-peaks", "∧", "Peaks", draft.spectrum_peaks, cx,
                            |draft| draft.spectrum_peaks = !draft.spectrum_peaks))
                        .child(visual_switch("spectrum-grid", "#", "Faint grid", draft.spectrum_grid, cx,
                            |draft| draft.spectrum_grid = !draft.spectrum_grid))
                        .child(visual_switch("spectrum-labels", "T", "Labels", draft.spectrum_labels, cx,
                            |draft| draft.spectrum_labels = !draft.spectrum_labels)))
                    .child(caption("Auto uses bar width plus gap, up to 512 bars. Gravity accelerates falling levels; 0 snaps to the signal after hold, bypassing smoothing. Attacks stay immediate. Labels show four dB levels and the first/last frequencies."))
            }
            VisualPage::Spectrogram => column().gap_3()
                .child(settings.pair(Field::SpectrogramMinHz, Field::SpectrogramMaxHz))
                .child(settings.pair(Field::SpectrogramDb, Field::SpectrogramHistory))
                .child(row().flex_wrap()
                    .child(visual_switch("spectrogram-scale", "ln", "Logarithmic frequency axis", draft.spectrogram_log_scale, cx,
                        |draft| draft.spectrogram_log_scale = !draft.spectrogram_log_scale))
                    .child(visual_switch("spectrogram-labels", "T", "Labels", draft.spectrogram_labels, cx,
                        |draft| draft.spectrogram_labels = !draft.spectrogram_labels)))
                .child(caption("History is measured in audio seconds, independent of refresh rate and panel width.")),
            VisualPage::Waveform => column().gap_3()
                .child(settings.field(Field::CursorColor))
                .child(settings.field(Field::Glow))
                .child(visual_switch("waveform-labels", "T", "Labels", draft.waveform_labels, cx,
                    |draft| draft.waveform_labels = !draft.waveform_labels))
                .child(caption("Appearance only: changing colors does not decode the track again. The waterline stays still while paused.")),
        }
    }

    pub(super) fn settings_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let category = row()
            .flex_wrap()
            .flex_shrink_0()
            .gap_1()
            .px(gpui::px(UI_INSET))
            .py(gpui::px(UI_INSET))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(self.settings.tab_button(
                "settings-general",
                "General",
                self.settings.page == SettingsPage::General,
                cx,
                |settings| settings.page = SettingsPage::General,
            ))
            .child(self.settings.tab_button(
                "settings-visualizations",
                "Visualizations",
                self.settings.page == SettingsPage::Visualizations,
                cx,
                |settings| settings.page = SettingsPage::Visualizations,
            ));
        let mut panel = column()
            .id(("settings-panel", panel_id))
            .relative()
            .w_full()
            .flex_1()
            .min_h_0()
            .gap_0()
            .child(category);
        let page_id = match (self.settings.page, self.settings.visual_page) {
            (SettingsPage::General, _) => "settings-general-scroll",
            (_, VisualPage::Common) => "settings-common-scroll",
            (_, VisualPage::Spectrum) => "settings-spectrum-scroll",
            (_, VisualPage::Spectrogram) => "settings-spectrogram-scroll",
            (_, VisualPage::Waveform) => "settings-waveform-scroll",
        };
        let content = if self.settings.page == SettingsPage::General {
            self.general_settings(cx)
        } else {
            panel = panel.child(self.visual_settings_tabs(cx));
            self.visual_settings(cx)
        };
        panel = panel.child(
            div()
                .id(page_id)
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(content.flex_shrink_0().p(gpui::px(UI_INSET))),
        );
        let feedback = if self.settings.applying {
            caption("Applying settings…").into_any_element()
        } else {
            match &self.settings.feedback {
                Some(Err(error)) => copyable_message("copy-settings-error", error.clone(), cx)
                    .text_color(rgb(ERROR))
                    .into_any_element(),
                Some(Ok(())) => caption("Last apply succeeded").into_any_element(),
                None => div().into_any_element(),
            }
        };
        panel = panel.child(
            row()
                .flex_shrink_0()
                .px(gpui::px(UI_INSET))
                .py(gpui::px(UI_INSET))
                .min_h(rems(3.))
                .border_t_1()
                .border_color(rgb(BORDER))
                .child(div().flex_1().min_w_0().text_sm().child(feedback))
                .child(
                    icon_button(
                        "settings-apply",
                        "✓",
                        "Apply settings",
                        cx,
                        |this, _, cx| this.apply_settings(cx),
                    )
                    .opacity(if self.settings.applying { 0.4 } else { 1.0 }),
                )
                .child(icon_button(
                    "settings-discard",
                    "×",
                    "Discard changes and close",
                    cx,
                    |this, _, cx| {
                        this.load_settings(cx);
                        this.settings_open = false;
                        cx.notify();
                    },
                )),
        );
        if self.settings.device_dropdown.is_open() {
            let bounds = self.measured.borrow().get(&Measured::Device).copied();
            if let Some(bounds) = bounds {
                let selected = self.settings.device_dropdown.selected_index().unwrap_or(0);
                let item_count = self.settings.device_dropdown.filtered().count();
                let height = (item_count as f32 * TRACK_HEIGHT)
                    .min(POPOVER_MAX_HEIGHT)
                    .min(f32::from(window.viewport_size().height) * 0.45);
                let devices = uniform_list(
                    "settings-devices",
                    item_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let (item_index, item) =
                                    this.settings.device_dropdown.filtered_item(index)?;
                                let label = item.label.clone();
                                let hint: SharedString = label.clone().into();
                                Some(
                                    dropdown_row(
                                        ("output-device", item_index),
                                        item_index == selected,
                                        label,
                                    )
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| ButtonTooltip { text: hint.clone() }).into()
                                    })
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.choose_device(item_index, cx),
                                    )),
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.settings.device_scroll)
                .h(px(height))
                .w_full();
                panel = panel.child(
                    deferred(
                        anchored()
                            .position(bounds.bottom_left())
                            .snap_to_window()
                            .child(
                                dropdown_container("device-dropdown", bounds.size.width)
                                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                        this.settings.device_dropdown.close();
                                        cx.notify();
                                    }))
                                    .child(devices),
                            ),
                    )
                    .with_priority(2),
                );
            }
        }
        panel.into_any_element()
    }
}
