use super::{
    ACCENT, BORDER, ButtonTooltip, DropdownItem, DropdownState, ERROR, GuiApp, HIGHLIGHT, Measured,
    POPOVER_MAX_HEIGHT, UI_INSET, artwork::Artwork, button, caption, column, copyable_message,
    dropdown_container, dropdown_row, dropdown_trigger, icon_button, input::Input,
    panels::TRACK_HEIGHT, row,
};
use crate::{
    config::{Config, LogLevel, RadialSpectrumStyle, RgbColor, SpectrumStyle, SpectrumWindow},
    model::{Command, RepeatMode},
};
use anyhow::{Context as _, Result};
use gpui::{prelude::*, *};
use std::{collections::HashMap, path::PathBuf, str::FromStr};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingChoice {
    Device(usize),
    Font(&'static str),
    SpectrumStyle(SpectrumStyle),
    Fft(u32),
    RadialSpectrumStyle(RadialSpectrumStyle),
    LogLevel(LogLevel),
    Window(SpectrumWindow),
}

impl SettingChoice {
    fn label(self) -> String {
        match self {
            Self::Device(_) => String::new(),
            Self::Font(value) => value.to_owned(),
            Self::SpectrumStyle(value) => spectrum_style_label(value).to_owned(),
            Self::RadialSpectrumStyle(value) => radial_spectrum_style_label(value).to_owned(),
            Self::Fft(value) => value.to_string(),
            Self::LogLevel(value) => match value {
                LogLevel::Debug => "Debug".to_owned(),
                LogLevel::Info => "Info".to_owned(),
                LogLevel::Warning => "Warning".to_owned(),
                LogLevel::Error => "Error".to_owned(),
            },
            Self::Window(SpectrumWindow::Hann) => "Hann".to_owned(),
            Self::Window(SpectrumWindow::BlackmanHarris) => "Blackman–Harris".to_owned(),
            Self::Window(SpectrumWindow::None) => "None".to_owned(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Roots,
    ScanMaxDepth,
    Volume,
    PlayCountThreshold,
    QueueLimit,
    PageSize,
    LogRetentionWeeks,
    Font,
    Scale,
    Fps,
    Background,
    RadialSpectrumSensitivity,
    RadialSpectrumRotationSpeed,
    RadialSpectrumBarWidth,
    RadialSpectrumBarGlowLayers,
    RadialSpectrumRingOpacity,
    RadialSpectrumBloomIntensity,
    RadialSpectrumInnerDiameter,
    RadialSpectrumPrimaryColor,
    RadialSpectrumSecondaryColor,
    SpectrumDb,
    SpectrumBarWidth,
    SpectrumBars,
    SpectrumGap,
    SpectrumHold,
    SpectrumGravity,
    SpectrumBarHold,
    SpectrumBarGravity,
    SpectrumSmoothing,
    SpectrogramDb,
    SpectrogramHistory,
    SpectrogramSamplingPointsScale,
    SpectrogramInterpolationPoints,
    CursorColor,
    Glow,

}
impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::ScanMaxDepth => "Scan directory depth (1+; roots are depth 0)",
            Self::Roots => "Library roots (separate paths with semicolons)",
            Self::Volume => "Volume (0–100%)",
            Self::PlayCountThreshold => "Play count threshold (%)",
            Self::QueueLimit => "Queue limit (1–4096)",
            Self::PageSize => "List batch size (20–256)",
            Self::LogRetentionWeeks => "Log retention (1–520 weeks)",
            Self::Font => "Interface font family (sans-serif, serif, monospace, or installed name)",
            Self::Scale => "Interface scale (0.75–2)",
            Self::Fps => "Analysis refresh rate (5–60 fps)",
            Self::Background => "Background (#RRGGBB)",
            Self::RadialSpectrumSensitivity => "Sensitivity (0–5)",
            Self::RadialSpectrumRotationSpeed => "Rotation speed (0–10)",
            Self::RadialSpectrumBarWidth => "Bar width (0–2)",
            Self::RadialSpectrumBarGlowLayers => "Bar glow layers (0–4)",
            Self::RadialSpectrumRingOpacity => "Ring opacity (0–1)",
            Self::RadialSpectrumBloomIntensity => "Bloom intensity (0–2)",
            Self::RadialSpectrumInnerDiameter => "Inner diameter (0–2)",
            Self::RadialSpectrumPrimaryColor | Self::RadialSpectrumSecondaryColor => {
                "Radial Spectrum color (#RRGGBB)"
            }
            Self::SpectrumDb => "Dynamic range (1–160 dB)",
            Self::SpectrogramDb => "Dynamic range (1–160 dB)",
            Self::SpectrumBarWidth => "Auto bar width (1–20 px)",
            Self::SpectrumBars => "Bar count (0 = auto; 1–512)",
            Self::SpectrumGap => "Bar gap (0–8 px)",
            Self::SpectrumHold => "Peak hold (0–2000 ms)",
            Self::SpectrumGravity => "Peak gravity (0–500 dB/s²)",
            Self::SpectrumBarHold => "Bar hold (0–2000 ms)",
            Self::SpectrumBarGravity => "Bar gravity (0–500 dB/s²)",
            Self::SpectrumSmoothing => "Release smoothing (0–1000 ms)",
            Self::SpectrogramHistory => "History limit (1–120 seconds)",
            Self::SpectrogramSamplingPointsScale => "Frequency point scale (0.4–1.4)",
            Self::SpectrogramInterpolationPoints => "Interpolated frequency points (64–4096)",
            Self::CursorColor => "Waterline color (#RRGGBB)",
            Self::Glow => "Glow strength (0–2; 0 = off)",

        }
    }
}

fn dropdown_button(
    id: &'static str,
    label: impl Into<SharedString>,
    cx: &mut Context<GuiApp>,
    action: impl Fn(&mut GuiApp, &mut Window, &mut Context<GuiApp>) + 'static,
) -> Stateful<Div> {
    dropdown_trigger(id, label).on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        cx.stop_propagation();
    }))
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

fn spectrum_style_label(style: SpectrumStyle) -> &'static str {
    match style {
        SpectrumStyle::Bars => "Bars",
        SpectrumStyle::Outline => "Outline",
        SpectrumStyle::Led => "LED",
        SpectrumStyle::Line => "Line",
        SpectrumStyle::Solid => "Solid",
    }
}
fn radial_spectrum_style_label(style: RadialSpectrumStyle) -> &'static str {
    match style {
        RadialSpectrumStyle::Bars => "Bars",
        RadialSpectrumStyle::Rings => "Rings",
        RadialSpectrumStyle::BarsRings => "Bars + Rings",
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
    About,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualPage {
    Common,
    Spectrum,
    Spectrogram,
    RadialSpectrum,
    Waveform,
}

/// Unsaved preferences stay independent from live playback snapshots.
pub(super) struct Settings {
    draft: Config,
    inputs: HashMap<Field, Entity<Input>>,
    initialized: bool,
    dropdown: DropdownState<SettingChoice>,
    device_scroll: UniformListScrollHandle,
    dropdown_anchor: Measured,
    applying: bool,
    session: u64,
    feedback: Option<std::result::Result<(), String>>,
    page: SettingsPage,
    visual_page: VisualPage,
}

impl Settings {
    pub(super) fn new(config: &Config) -> Self {
        Self {
            draft: config.clone(),
            inputs: HashMap::new(),
            initialized: false,
            dropdown: DropdownState::default(),
            device_scroll: UniformListScrollHandle::new(),
            dropdown_anchor: Measured::Device,
            applying: false,
            session: 0,
            feedback: None,
            page: SettingsPage::General,
            visual_page: VisualPage::Common,
        }
    }

    pub(super) fn initialize(&mut self, cx: &mut App) {
        if self.initialized {
            return;
        }
        self.inputs = [
            Field::Roots,
            Field::ScanMaxDepth,
            Field::Volume,
            Field::PlayCountThreshold,
            Field::QueueLimit,
            Field::PageSize,
            Field::LogRetentionWeeks,
            Field::Font,
            Field::Scale,
            Field::Fps,
            Field::Background,
            Field::RadialSpectrumSensitivity,
            Field::RadialSpectrumRotationSpeed,
            Field::RadialSpectrumBarWidth,
            Field::RadialSpectrumBarGlowLayers,
            Field::RadialSpectrumRingOpacity,
            Field::RadialSpectrumBloomIntensity,
            Field::RadialSpectrumInnerDiameter,
            Field::RadialSpectrumPrimaryColor,
            Field::RadialSpectrumSecondaryColor,
            Field::SpectrumDb,
            Field::SpectrumBarWidth,
            Field::SpectrumBars,
            Field::SpectrumGap,
            Field::SpectrumHold,
            Field::SpectrumGravity,
            Field::SpectrumBarHold,
            Field::SpectrumBarGravity,
            Field::SpectrumSmoothing,
            Field::SpectrogramDb,
            Field::SpectrogramHistory,
            Field::SpectrogramSamplingPointsScale,
            Field::SpectrogramInterpolationPoints,
            Field::CursorColor,
            Field::Glow,
        ]
        .into_iter()
        .map(|field| (field, cx.new(|cx| Input::new("", field.label(), cx))))
        .collect();
        self.initialized = true;
        self.fill(cx);
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
        self.set_value(
            Field::ScanMaxDepth,
            self.draft.scan_max_depth.to_string(),
            cx,
        );
        self.set_value(Field::Volume, (self.draft.volume * 100.).to_string(), cx);
        self.set_value(
            Field::PlayCountThreshold,
            self.draft.play_count_threshold_percent.to_string(),
            cx,
        );
        self.set_value(Field::QueueLimit, self.draft.queue_limit.to_string(), cx);
        self.set_value(Field::PageSize, self.draft.page_size.to_string(), cx);
        self.set_value(
            Field::LogRetentionWeeks,
            self.draft.log_retention_weeks.to_string(),
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
            Field::RadialSpectrumSensitivity,
            self.draft.radial_spectrum_sensitivity.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumRotationSpeed,
            self.draft.radial_spectrum_rotation_speed.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumBarWidth,
            self.draft.radial_spectrum_bar_width.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumBarGlowLayers,
            self.draft.radial_spectrum_bar_glow_layers.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumRingOpacity,
            self.draft.radial_spectrum_ring_opacity.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumBloomIntensity,
            self.draft.radial_spectrum_bloom_intensity.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumInnerDiameter,
            self.draft.radial_spectrum_inner_diameter.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumPrimaryColor,
            self.draft.radial_spectrum_primary_color.to_string(),
            cx,
        );
        self.set_value(
            Field::RadialSpectrumSecondaryColor,
            self.draft.radial_spectrum_secondary_color.to_string(),
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
            Field::SpectrogramSamplingPointsScale,
            self.draft.spectrogram_sampling_points_scale.to_string(),
            cx,
        );
        self.set_value(
            Field::SpectrogramInterpolationPoints,
            self.draft.spectrogram_interpolation_points.to_string(),
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
        config.scan_max_depth = self.number(Field::ScanMaxDepth, cx)?;
        config.volume = self.number::<f32>(Field::Volume, cx)? / 100.;
        config.play_count_threshold_percent = self.number(Field::PlayCountThreshold, cx)?;
        config.queue_limit = self.number(Field::QueueLimit, cx)?;
        config.page_size = self.number(Field::PageSize, cx)?;
        config.log_retention_weeks = self.number(Field::LogRetentionWeeks, cx)?;
        config.ui_font = self.value(Field::Font, cx).trim().to_owned();
        config.ui_scale = self.number(Field::Scale, cx)?;
        config.analysis_fps = self.number(Field::Fps, cx)?;
        config.visual_background = self
            .value(Field::Background, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Visualization background")?;
        config.radial_spectrum_sensitivity = self.number(Field::RadialSpectrumSensitivity, cx)?;
        config.radial_spectrum_rotation_speed =
            self.number(Field::RadialSpectrumRotationSpeed, cx)?;
        config.radial_spectrum_bar_width = self.number(Field::RadialSpectrumBarWidth, cx)?;
        config.radial_spectrum_bar_glow_layers =
            self.number(Field::RadialSpectrumBarGlowLayers, cx)?;
        config.radial_spectrum_ring_opacity = self.number(Field::RadialSpectrumRingOpacity, cx)?;
        config.radial_spectrum_bloom_intensity =
            self.number(Field::RadialSpectrumBloomIntensity, cx)?;
        config.radial_spectrum_inner_diameter =
            self.number(Field::RadialSpectrumInnerDiameter, cx)?;
        config.radial_spectrum_primary_color = self
            .value(Field::RadialSpectrumPrimaryColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Radial Spectrum primary color")?;
        config.radial_spectrum_secondary_color = self
            .value(Field::RadialSpectrumSecondaryColor, cx)
            .trim()
            .parse::<RgbColor>()
            .context("Radial Spectrum secondary color")?;
        config.spectrum_db_range = self.number(Field::SpectrumDb, cx)?;
        config.spectrum_gap = self.number(Field::SpectrumGap, cx)?;
        config.spectrum_peak_hold_ms = self.number(Field::SpectrumHold, cx)?;
        config.spectrum_peak_gravity = self.number(Field::SpectrumGravity, cx)?;
        config.spectrum_bar_hold_ms = self.number(Field::SpectrumBarHold, cx)?;
        config.spectrum_bar_gravity = self.number(Field::SpectrumBarGravity, cx)?;
        config.spectrum_smoothing_ms = self.number(Field::SpectrumSmoothing, cx)?;
        config.spectrum_bar_width = self.number(Field::SpectrumBarWidth, cx)?;
        config.spectrogram_db_range = self.number(Field::SpectrogramDb, cx)?;
        config.spectrogram_history_seconds = self.number(Field::SpectrogramHistory, cx)?;
        config.spectrogram_sampling_points_scale =
            self.number(Field::SpectrogramSamplingPointsScale, cx)?;
        config.spectrogram_interpolation_points =
            self.number(Field::SpectrogramInterpolationPoints, cx)?;
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
            this.settings.dropdown.close();
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
        let config = self.handle.config_snapshot();
        self.settings.initialize(cx);
        self.settings.dropdown.close();
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
                .spawn(async move { handle.request_ack(Command::Configure { config }) })
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
        let Some((_, item)) = self.settings.dropdown.filtered_item(index) else {
            return;
        };
        match item.value {
            SettingChoice::Device(device_index) => {
                if device_index == 0 {
                    self.settings.draft.output_device = None;
                } else if let Some(name) = self.state.system.devices.get(device_index - 1) {
                    self.settings.draft.output_device = Some(name.clone());
                } else {
                    return;
                }
            }
            SettingChoice::Font(value) => {
                self.settings.draft.ui_font = value.to_owned();
                self.settings.inputs[&Field::Font].update(cx, |input, cx| {
                    input.set_text(value, cx);
                });
            }
            SettingChoice::SpectrumStyle(value) => self.settings.draft.spectrum_style = value,
            SettingChoice::RadialSpectrumStyle(value) => {
                self.settings.draft.radial_spectrum_style = value
            }
            SettingChoice::Fft(value) => self.settings.draft.spectrum_fft_size = value,
            SettingChoice::LogLevel(value) => self.settings.draft.log_level = value,
            SettingChoice::Window(value) => self.settings.draft.spectrum_window = value,
        }
        self.settings.dropdown.select_index(index);
        self.settings.dropdown.close();
        cx.notify();
    }

    fn open_setting_dropdown(&mut self, items: Vec<DropdownItem<SettingChoice>>, selected: usize) {
        if let Some(item) = items.first() {
            self.settings.dropdown_anchor = match item.value {
                SettingChoice::Font(_) => Measured::SettingsFont,
                SettingChoice::SpectrumStyle(_) => Measured::SettingsStyle,
                SettingChoice::Fft(_) => Measured::SettingsFft,
                SettingChoice::LogLevel(_) => Measured::SettingsLogLevel,
                SettingChoice::Window(_) => Measured::SettingsWindow,
                SettingChoice::RadialSpectrumStyle(_) => Measured::SettingsStyle,
                SettingChoice::Device(_) => Measured::Device,
            };
        }
        self.settings.dropdown.open(items);
        self.settings.dropdown.select_index(selected);
    }

    pub(super) fn settings_dropdown_is_open(&self) -> bool {
        self.settings.dropdown.is_open()
    }

    pub(super) fn close_settings_dropdown(&mut self) {
        self.settings.dropdown.close();
    }

    pub(super) fn settings_dropdown_key(&mut self, key: &str, cx: &mut Context<Self>) {
        match key {
            "up" => self.settings.dropdown.move_previous(),
            "down" => self.settings.dropdown.move_next(),
            "enter" | "space" => {
                if let Some(index) = self.settings.dropdown.selected_index() {
                    self.choose_device(index, cx);
                }
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        if let Some(index) = self.settings.dropdown.selected_index() {
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
                dropdown_trigger("output-device-selector", chosen_device.clone())
                    .relative()
                    .tooltip({
                        let chosen_device = chosen_device.clone();
                        move |_, cx| {
                            cx.new(|_| ButtonTooltip {
                                text: chosen_device.clone(),
                            })
                            .into()
                        }
                    })
                    .child(self.measurement(Measured::Device))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let open = this.settings.dropdown.is_open();
                        if open {
                            this.settings.dropdown.close();
                        } else {
                            let selected = this
                                .settings
                                .draft
                                .output_device
                                .as_ref()
                                .and_then(|name| {
                                    this.state
                                        .system
                                        .devices
                                        .iter()
                                        .position(|device| device == name)
                                })
                                .map_or(0, |index| index + 1);
                            let items = std::iter::once(DropdownItem::new(
                                SettingChoice::Device(0),
                                "System default",
                            ))
                            .chain(
                                this.state.system.devices.iter().enumerate().map(
                                    |(index, device)| {
                                        DropdownItem::new(
                                            SettingChoice::Device(index + 1),
                                            device.clone(),
                                        )
                                    },
                                ),
                            );
                            this.settings.dropdown.open(items);
                            this.settings.dropdown.select_index(selected);
                        }
                        if let Some(index) = this.settings.dropdown.selected_index() {
                            this.settings
                                .device_scroll
                                .scroll_to_item(index, ScrollStrategy::Nearest);
                        }
                        this.settings_focus.focus(window, cx);
                        cx.notify();
                    })),
            );
        let draft = &self.settings.draft;
        let (shuffle, repeat, mpris, tray, ffmpeg, remix) = (
            draft.shuffle,
            draft.repeat,
            draft.mpris_enabled,
            draft.tray_enabled,
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
                    format!("Media controls (MPRIS)\n{}", self.state.system.mpris_status),
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
                icon_button("settings-tray", "▣", "System tray", cx, |this, _, cx| {
                    this.settings.draft.tray_enabled = !this.settings.draft.tray_enabled;
                    cx.notify();
                })
                .size(rems(4.))
                .text_size(rems(1.75))
                .when(tray, |view| view.bg(rgb(HIGHLIGHT)).text_color(rgb(ACCENT))),
            )
            .child(
                icon_button(
                    "settings-ffmpeg",
                    "\u{f384}",
                    ffmpeg_hint(&self.state.system.ffmpeg_status),
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
        let font = self.settings.draft.ui_font.clone();
        let font_dropdown = dropdown_button(
            "settings-font",
            if font.is_empty() {
                "system".to_owned()
            } else {
                font
            },
            cx,
            |this, _, _| {
                let values = ["", "sans-serif", "serif", "monospace"];
                let selected = values
                    .iter()
                    .position(|value| *value == this.settings.draft.ui_font)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            DropdownItem::new(
                                SettingChoice::Font(value),
                                if value.is_empty() {
                                    "system".to_owned()
                                } else {
                                    value.to_owned()
                                },
                            )
                        })
                        .collect(),
                    selected,
                );
            },
        );
        let log_level_dropdown = dropdown_button(
            "settings-log-level",
            SettingChoice::LogLevel(draft.log_level).label(),
            cx,
            |this, _, _| {
                let values = [
                    LogLevel::Debug,
                    LogLevel::Info,
                    LogLevel::Warning,
                    LogLevel::Error,
                ];
                let selected = values
                    .iter()
                    .position(|value| *value == this.settings.draft.log_level)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            DropdownItem::new(
                                SettingChoice::LogLevel(value),
                                SettingChoice::LogLevel(value).label(),
                            )
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::SettingsLogLevel));
        let logging = column().gap_2().child(caption("Logging")).child(
            row()
                .items_start()
                .flex_wrap()
                .gap_3()
                .child(
                    column()
                        .flex_1()
                        .min_w(px(180.))
                        .gap_1()
                        .child(caption("Log level"))
                        .child(log_level_dropdown),
                )
                .child(
                    self.settings
                        .field(Field::LogRetentionWeeks)
                        .flex_1()
                        .min_w(px(180.)),
                )
                .child(visual_switch(
                    "settings-log-file",
                    "▤",
                    "Write logs to disk",
                    draft.log_to_file,
                    cx,
                    |draft| draft.log_to_file = !draft.log_to_file,
                )),
        );

        column()
            .gap_3()
            .child(self.settings.field(Field::Roots))
            .child(self.settings.field(Field::ScanMaxDepth))
            .child(device)
            .child(self.settings.pair(Field::Volume, Field::PlayCountThreshold))
            .child(self.settings.pair(Field::QueueLimit, Field::PageSize))
            .child(caption("Interface font"))
            .child(
                font_dropdown
                    .relative()
                    .child(self.measurement(Measured::SettingsFont)),
            )
            .child(self.settings.field(Field::Scale))
            .child(switches)
            .child(logging)
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
                "visual-spectrogram",
                "Spectrogram",
                selected == VisualPage::Spectrogram,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrogram,
            ))
            .child(self.settings.tab_button(
                "visual-spectrum",
                "Spectrum",
                selected == VisualPage::Spectrum,
                cx,
                |settings| settings.visual_page = VisualPage::Spectrum,
            ))
            .child(self.settings.tab_button(
                "visual-radial-spectrum",
                "Radial Spectrum",
                selected == VisualPage::RadialSpectrum,
                cx,
                |settings| settings.visual_page = VisualPage::RadialSpectrum,
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
                .child(settings.pair(Field::Fps, Field::Background)),
            VisualPage::Spectrum => {
                let style = spectrum_style_label(draft.spectrum_style);
                column()
                    .gap_3()
                    .child(caption("Style"))
                    .child(dropdown_button("spectrum-style", style, cx, |this, _, _| {
                        let values = [SpectrumStyle::Bars, SpectrumStyle::Outline, SpectrumStyle::Led, SpectrumStyle::Line, SpectrumStyle::Solid];
                        let selected = values.iter().position(|value| *value == this.settings.draft.spectrum_style).unwrap_or(0);
                        this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::SpectrumStyle(value), spectrum_style_label(value))).collect(), selected);
                    }).relative().child(self.measurement(Measured::SettingsStyle)))
                    .child(caption("FFT size"))
                    .child(dropdown_button("spectrum-fft", draft.spectrum_fft_size.to_string(), cx, |this, _, _| {
                        let values = [512, 1024, 2048, 4096, 8192, 16384, 32768];
                        let selected = values.iter().position(|value| *value == this.settings.draft.spectrum_fft_size).unwrap_or(0);
                        this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::Fft(value), value.to_string())).collect(), selected);
                    }).relative().child(self.measurement(Measured::SettingsFft)))
                    .child(caption("Window"))
                    .child(dropdown_button("spectrum-window", SettingChoice::Window(draft.spectrum_window).label(), cx, |this, _, _| {
                        let values = [SpectrumWindow::Hann, SpectrumWindow::BlackmanHarris, SpectrumWindow::None];
                        let selected = values.iter().position(|value| *value == this.settings.draft.spectrum_window).unwrap_or(0);
                        this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::Window(value), SettingChoice::Window(value).label())).collect(), selected);
                    }).relative().child(self.measurement(Measured::SettingsWindow)))
                    .child(settings.pair(Field::SpectrumDb, Field::SpectrumBarWidth))
                    .child(settings.pair(Field::SpectrumBars, Field::SpectrumGap))
                    .child(settings.field(Field::SpectrumSmoothing))
                    .child(settings.pair(Field::SpectrumBarHold, Field::SpectrumBarGravity))
                    .child(settings.pair(Field::SpectrumHold, Field::SpectrumGravity))
                    .child(row().flex_wrap()
                        .child(visual_switch("spectrum-interpolate", "≈", "Interpolation", draft.spectrum_interpolate, cx,
                            |draft| draft.spectrum_interpolate = !draft.spectrum_interpolate))
                        .child(visual_switch("spectrum-peaks", "∧", "Peaks", draft.spectrum_peaks, cx,
                            |draft| draft.spectrum_peaks = !draft.spectrum_peaks))
                        .child(visual_switch("spectrum-grid", "#", "Faint grid", draft.spectrum_grid, cx,
                            |draft| draft.spectrum_grid = !draft.spectrum_grid))
                        .child(visual_switch("spectrum-labels", "T", "Labels", draft.spectrum_labels, cx,
                            |draft| draft.spectrum_labels = !draft.spectrum_labels)))
                    .child(caption("Auto uses bar width plus gap, up to 512 bars. Gravity accelerates falling levels; 0 snaps to the signal after hold, bypassing smoothing. Attacks stay immediate. Labels show four dB levels and the first/last frequencies."))
            }
            VisualPage::Spectrogram => column()
                .gap_3()
                .child(settings.pair(Field::SpectrogramDb, Field::SpectrogramHistory))
                .child(settings.pair(
                    Field::SpectrogramSamplingPointsScale,
                    Field::SpectrogramInterpolationPoints,
                ))
                .child(
                    row()
                        .flex_wrap()
                        .child(visual_switch(
                            "spectrogram-interpolate",
                            "≈",
                            "Interpolation",
                            draft.spectrogram_interpolate,
                            cx,
                            |draft| draft.spectrogram_interpolate = !draft.spectrogram_interpolate,
                        ))
                        .child(visual_switch(
                            "spectrogram-labels",
                            "T",
                            "Labels",
                            draft.spectrogram_labels,
                            cx,
                            |draft| draft.spectrogram_labels = !draft.spectrogram_labels,
                        )),
                )
                .child(caption(
                    "The frequency axis is logarithmic from 16 Hz to Nyquist; the tallest active Spectrogram panel sets the shared source resolution, capped by the configured interpolation points, and smaller panels scale it down.",
                )),
            VisualPage::RadialSpectrum => {
                column()
                    .gap_3()
                    .child(caption("Style"))
                    .child(dropdown_button(
                        "radial-spectrum-style",
                        radial_spectrum_style_label(draft.radial_spectrum_style),
                        cx,
                        |this, _, _| {
                            let values = [
                                RadialSpectrumStyle::Bars,
                                RadialSpectrumStyle::Rings,
                                RadialSpectrumStyle::BarsRings,
                            ];
                            let selected = values
                                .iter()
                                .position(|value| *value == this.settings.draft.radial_spectrum_style)
                                .unwrap_or(0);
                            this.open_setting_dropdown(
                                values
                                    .into_iter()
                                    .map(|value| {
                                        DropdownItem::new(
                                            SettingChoice::RadialSpectrumStyle(value),
                                            radial_spectrum_style_label(value),
                                        )
                                    })
                                    .collect(),
                                selected,
                            );
                        },
                    ))
                    .child(settings.pair(Field::RadialSpectrumSensitivity, Field::RadialSpectrumRotationSpeed))
                    .child(settings.pair(Field::RadialSpectrumBarWidth, Field::RadialSpectrumBarGlowLayers))
                    .child(settings.pair(Field::RadialSpectrumRingOpacity, Field::RadialSpectrumBloomIntensity))
                    .child(settings.pair(Field::RadialSpectrumPrimaryColor, Field::RadialSpectrumSecondaryColor))
                    .child(visual_switch(
                        "radial-spectrum-fade-idle",
                        "◌",
                        "Fade when idle",
                        draft.radial_spectrum_fade_when_idle,
                        cx,
                        |draft| draft.radial_spectrum_fade_when_idle = !draft.radial_spectrum_fade_when_idle,
                    ))
                    .child(caption("Radial Spectrum follows the Noctalia v5 Fancy Audio Visualizer's Bars/Rings control. Rivu uses the shared FFT and GPUI-native rendering. Fade when idle uses a 2-second opacity fade."))
            }
            VisualPage::Waveform => column().gap_3()
                .child(settings.field(Field::CursorColor))
                .child(settings.field(Field::Glow))
                .child(visual_switch("waveform-labels", "T", "Labels", draft.waveform_labels, cx,
                    |draft| draft.waveform_labels = !draft.waveform_labels))
                .child(caption("Appearance only: changing colors does not decode the track again. The waterline stays still while paused.")),
        }
    }

    fn about_settings(&self, cx: &mut Context<GuiApp>) -> Div {
        column()
            .w_full()
            .gap_3()
            .items_center()
            .text_center()
            .child(
                div()
                    .mt(px(20.))
                    .size(px(128.))
                    .child(cx.new(|_| Artwork::new())),
            )
            .child(caption("rivu, a local-first music player"))
            .child(caption(format!("Version {}", env!("CARGO_PKG_VERSION"))))
            .child(caption("GPL-3.0-or-later"))
    }

    pub(super) fn settings_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.settings.initialize(cx);
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
            ))
            .child(self.settings.tab_button(
                "settings-about",
                "About",
                self.settings.page == SettingsPage::About,
                cx,
                |settings| settings.page = SettingsPage::About,
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
            (SettingsPage::About, _) => "settings-about-scroll",
            (_, VisualPage::Common) => "settings-common-scroll",
            (_, VisualPage::Spectrum) => "settings-spectrum-scroll",
            (_, VisualPage::Spectrogram) => "settings-spectrogram-scroll",
            (_, VisualPage::RadialSpectrum) => "settings-radial-spectrum-scroll",
            (_, VisualPage::Waveform) => "settings-waveform-scroll",
        };
        let content = match self.settings.page {
            SettingsPage::General => self.general_settings(cx),
            SettingsPage::Visualizations => {
                panel = panel.child(self.visual_settings_tabs(cx));
                self.visual_settings(cx)
            }
            SettingsPage::About => self.about_settings(cx),
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
        if self.settings.dropdown.is_open() {
            let bounds = self
                .measured
                .borrow()
                .get(&self.settings.dropdown_anchor)
                .copied();
            if let Some(bounds) = bounds {
                let selected = self.settings.dropdown.selected_index().unwrap_or(0);
                let item_count = self.settings.dropdown.filtered().count();
                let height = (item_count as f32 * TRACK_HEIGHT * self.state.system.config.ui_scale)
                    .min(POPOVER_MAX_HEIGHT * self.state.system.config.ui_scale)
                    .min(f32::from(window.viewport_size().height) * 0.45);
                let choices = uniform_list(
                    "settings-dropdown",
                    item_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let (item_index, item) =
                                    this.settings.dropdown.filtered_item(index)?;
                                let label = item.label.clone();
                                let hint: SharedString = label.clone().into();
                                Some(
                                    dropdown_row(
                                        ("settings-choice", item_index),
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
                                dropdown_container("settings-dropdown", bounds.size.width)
                                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                        this.settings.dropdown.close();
                                        cx.notify();
                                    }))
                                    .child(choices),
                            ),
                    )
                    .with_priority(2),
                );
            }
        }
        panel.into_any_element()
    }
}
