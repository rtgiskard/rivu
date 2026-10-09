use super::{
    BORDER, ButtonTooltip, DropdownItem, DropdownState, ERROR, GuiApp, Measured,
    POPOVER_MAX_HEIGHT, StatusKind, UI_INSET, active_control, button, caption, column,
    components::DROPDOWN_ROW_REMS, copyable_message, dropdown_container, dropdown_row,
    dropdown_trigger, form_field, icon_button, input::Input, row, section_group, status_message,
    toggle_button, two_column,
};
use crate::{
    config::{
        Config, LogLevel, RadialSpectrumStyle, RgbColor, SpectrumStyle, SpectrumWindow,
        VisualizationPalette,
    },
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
    Palette(VisualizationPalette),
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
            Self::Palette(value) => visualization_palette_label(value).to_owned(),
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
    MediaReadBuffer,
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
    WaveformRmsGain,
    WaveformPeakGain,
    WaveformPeakGamma,
}
impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::ScanMaxDepth => "Scan depth (1+; scan start = 0)",
            Self::Roots => "Library roots (separate paths with semicolons)",
            Self::Volume => "Volume (0–100%)",
            Self::PlayCountThreshold => "Play count threshold (0–<100%)",
            Self::QueueLimit => "Queue limit (1–4096)",
            Self::PageSize => "List batch size (20–256)",
            Self::LogRetentionWeeks => "Log retention (1–520 weeks)",
            Self::MediaReadBuffer => "Read buffer (1, 2, 4, 8, 16 MiB)",
            Self::Font => "Font family (empty = system)",
            Self::Scale => "Interface scale (0.75–2)",
            Self::Fps => "Analysis rate (5–60 Hz)",
            Self::Background => "Background (#RRGGBB)",
            Self::RadialSpectrumSensitivity => "Sensitivity (0–5)",
            Self::RadialSpectrumRotationSpeed => "Rotation speed (0–10)",
            Self::RadialSpectrumBarWidth => "Bar width (0–2)",
            Self::RadialSpectrumBarGlowLayers => "Bar glow layers (0–4)",
            Self::RadialSpectrumRingOpacity => "Ring opacity (0–1)",
            Self::RadialSpectrumBloomIntensity => "Bloom intensity (0–2)",
            Self::RadialSpectrumInnerDiameter => "Inner diameter (0–2)",
            Self::RadialSpectrumPrimaryColor => "Primary color (#RRGGBB)",
            Self::RadialSpectrumSecondaryColor => "Secondary color (#RRGGBB)",
            Self::SpectrumDb => "Spectrum / Radial range (1–160 dB)",
            Self::SpectrogramDb => "Color range (1–160 dB)",
            Self::SpectrumBarWidth => "Auto bar width (1–20 px)",
            Self::SpectrumBars => "Bar count (0 = auto; 1–512)",
            Self::SpectrumGap => "Bar gap (0–8 px)",
            Self::SpectrumHold => "Peak hold (0–2000 ms)",
            Self::SpectrumGravity => "Peak gravity (0–500 dB/s²)",
            Self::SpectrumBarHold => "Bar hold (0–2000 ms)",
            Self::SpectrumBarGravity => "Bar gravity (0–500 dB/s²)",
            Self::SpectrumSmoothing => "Release smoothing (0–1000 ms)",
            Self::SpectrogramHistory => "History limit (1–120 seconds)",
            Self::SpectrogramSamplingPointsScale => "Source row scale (0.4–1.4)",
            Self::SpectrogramInterpolationPoints => "Maximum frequency rows (64–4096)",
            Self::CursorColor => "Waterline color (#RRGGBB)",
            Self::Glow => "Glow strength (0–2; 0 = off)",
            Self::WaveformRmsGain => "RMS gain (0–3)",
            Self::WaveformPeakGain => "Peak gain (0–2)",
            Self::WaveformPeakGamma => "Peak gamma (0.25–3)",
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
        this.settings_focus.focus(window, cx);
        cx.notify();
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
    toggle_button(id, icon, label, enabled, cx, move |this, _| {
        toggle(&mut this.settings.draft);
    })
}

fn settings_group(title: &'static str, content: impl IntoElement) -> Div {
    section_group(title, content)
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

fn visualization_palette_label(palette: VisualizationPalette) -> &'static str {
    match palette {
        VisualizationPalette::TokyoNight => "Tokyo Night",
        VisualizationPalette::Deadbeef => "Deadbeef",
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
    AudioVisualizer,
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
            Field::MediaReadBuffer,
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
            Field::WaveformRmsGain,
            Field::WaveformPeakGain,
            Field::WaveformPeakGamma,
        ]
        .into_iter()
        .map(|field| (field, cx.new(|cx| Input::new("", field.label(), cx))))
        .collect();
        self.initialized = true;
        self.fill(cx);
    }

    fn field(&self, field: Field) -> Div {
        form_field(field.label(), self.inputs[&field].clone())
    }

    fn pair(&self, first: Field, second: Field) -> Div {
        two_column(
            self.field(first).min_w(px(180.)),
            self.field(second).min_w(px(180.)),
        )
    }

    fn single(&self, field: Field) -> Div {
        row().child(self.field(field).w(relative(0.5)).min_w(px(180.)).pr_2())
    }

    fn field_state(&self, field: Field, enabled: bool, reason: &'static str, cx: &App) -> Div {
        if enabled {
            return self.field(field).min_w(px(180.));
        }
        // Keep the draft value, but unmount the editor so disabled fields cannot take focus or edits.
        let control = div()
            .id(("inactive-setting", field as usize))
            .h(px(36.))
            .p(px(7.))
            .line_height(px(20.))
            .text_size(px(14.))
            .bg(rgb(super::PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded(px(5.))
            .cursor_default()
            .child(self.value(field, cx))
            .tooltip(move |_, cx| {
                cx.new(|_| ButtonTooltip {
                    text: reason.into(),
                })
                .into()
            });
        form_field(field.label(), control)
            .min_w(px(180.))
            .opacity(0.45)
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
        self.set_value(
            Field::MediaReadBuffer,
            self.draft.media_read_buffer_mb.to_string(),
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
        self.set_value(
            Field::WaveformRmsGain,
            self.draft.waveform_rms_gain.to_string(),
            cx,
        );
        self.set_value(
            Field::WaveformPeakGain,
            self.draft.waveform_peak_gain.to_string(),
            cx,
        );
        self.set_value(
            Field::WaveformPeakGamma,
            self.draft.waveform_peak_gamma.to_string(),
            cx,
        );
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
        config.media_read_buffer_mb = self.number(Field::MediaReadBuffer, cx)?;
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
        config.spectrum_bars = self.number(Field::SpectrumBars, cx)?;
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
        config.waveform_rms_gain = self.number(Field::WaveformRmsGain, cx)?;
        config.waveform_peak_gain = self.number(Field::WaveformPeakGain, cx)?;
        config.waveform_peak_gamma = self.number(Field::WaveformPeakGamma, cx)?;
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
        active_control(
            button(id, label, cx, move |this, _, cx| {
                this.settings.dropdown.close();
                select(&mut this.settings);
                cx.notify();
            }),
            active,
        )
        .flex_shrink_0()
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
            SettingChoice::Palette(value) => self.settings.draft.visual_palette = value,
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
                SettingChoice::Palette(_) => Measured::SettingsStyle,
                SettingChoice::Device(_) => Measured::Device,
            };
        }
        self.settings.dropdown.open(items);
        self.settings.dropdown.select_index(selected);
        if let Some(index) = self.settings.dropdown.selected_index() {
            self.settings
                .device_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
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
                dropdown_button(
                    "output-device-selector",
                    chosen_device.clone(),
                    cx,
                    |this, _, _| {
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
                    },
                )
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
                .child(self.measurement(Measured::Device)),
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
        let font_dropdown = dropdown_button("settings-font", "Font presets", cx, |this, _, cx| {
            let values = ["", "sans-serif", "serif", "monospace"];
            let font = this.settings.inputs[&Field::Font].read(cx).text().trim();
            let selected = values.iter().position(|value| *value == font).unwrap_or(0);
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
        });
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
        let logging = settings_group(
            "Logging",
            row()
                .items_center()
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
        let library = settings_group(
            "Library",
            row()
                .items_start()
                .flex_wrap()
                .gap_3()
                .child(self.settings.field(Field::Roots).flex_1().min_w(px(260.)))
                .child(
                    self.settings
                        .field(Field::ScanMaxDepth)
                        .flex_1()
                        .min_w(px(220.)),
                ),
        );
        let playback = settings_group(
            "Playback",
            column()
                .gap_2()
                .child(self.settings.pair(Field::Volume, Field::PlayCountThreshold))
                .child(self.settings.single(Field::QueueLimit))
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .child(visual_switch(
                            "settings-shuffle",
                            "󰒟",
                            "Shuffle",
                            shuffle,
                            cx,
                            |draft| draft.shuffle = !draft.shuffle,
                        ))
                        .child(active_control(
                            icon_button(
                                "settings-repeat",
                                if repeat == RepeatMode::One {
                                    "󰑘"
                                } else {
                                    "󰑖"
                                },
                                match repeat {
                                    RepeatMode::Off => "Repeat: Off; click for All",
                                    RepeatMode::All => "Repeat: All; click for One",
                                    RepeatMode::One => "Repeat: One; click for Off",
                                },
                                cx,
                                |this, _, cx| {
                                    this.settings.draft.repeat = match this.settings.draft.repeat {
                                        RepeatMode::Off => RepeatMode::All,
                                        RepeatMode::All => RepeatMode::One,
                                        RepeatMode::One => RepeatMode::Off,
                                    };
                                    cx.notify();
                                },
                            ),
                            repeat != RepeatMode::Off,
                        )),
                ),
        );
        let audio = settings_group(
            "Audio",
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(device.flex_1().min_w(px(260.)))
                        .child(
                            self.settings
                                .field(Field::MediaReadBuffer)
                                .flex_1()
                                .min_w(px(240.)),
                        ),
                )
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .child(active_control(
                            icon_button(
                                "settings-ffmpeg",
                                "\u{f384}",
                                format!("FFmpeg decoding: {}\n{}", if ffmpeg { "On" } else { "Off" }, ffmpeg_hint(&self.state.system.ffmpeg_status)),
                                cx,
                                |this, _, cx| {
                                    this.settings.draft.ffmpeg_enabled = !this.settings.draft.ffmpeg_enabled;
                                    cx.notify();
                                },
                            )
                            .text_size(rems(1.)),
                            ffmpeg,
                        ))
                        .child(visual_switch(
                            "settings-pipewire-auto-mix",
                            "󱀞",
                            "PipeWire channel remix",
                            remix,
                            cx,
                            |draft| draft.pipewire_auto_mix = !draft.pipewire_auto_mix,
                        )),
                )
                .child(caption("Read buffer changes apply when media is next opened, not to the current track.")),
        );
        let interface = settings_group(
            "Interface",
            column()
                .gap_2()
                .child(
                    row()
                        .items_end()
                        .flex_wrap()
                        .gap_3()
                        .child(self.settings.field(Field::Font).flex_1().min_w(px(260.)))
                        .child(
                            font_dropdown
                                .min_w(px(140.))
                                .w_auto()
                                .relative()
                                .child(self.measurement(Measured::SettingsFont)),
                        ),
                )
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(self.settings.field(Field::Scale).flex_1().min_w(px(180.)))
                        .child(
                            self.settings
                                .field(Field::PageSize)
                                .flex_1()
                                .min_w(px(220.)),
                        ),
                )
                .child(row().child(visual_switch(
                    "settings-nerd-symbols",
                    "Nf",
                    "Nerd Font symbols",
                    draft.nerd_symbols,
                    cx,
                    |draft| draft.nerd_symbols = !draft.nerd_symbols,
                ))),
        );
        let desktop = settings_group(
            "Desktop integration",
            column()
                .gap_2()
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .child(active_control(
                            icon_button(
                                "settings-mpris",
                                "󰐹",
                                format!(
                                    "Media controls (MPRIS): {}\n{}",
                                    if mpris { "On" } else { "Off" },
                                    self.state.system.mpris_status
                                ),
                                cx,
                                |this, _, cx| {
                                    this.settings.draft.mpris_enabled =
                                        !this.settings.draft.mpris_enabled;
                                    cx.notify();
                                },
                            )
                            .text_size(rems(1.)),
                            mpris,
                        ))
                        .child(visual_switch(
                            "settings-tray",
                            "▣",
                            "System tray (restart required)",
                            tray,
                            cx,
                            |draft| draft.tray_enabled = !draft.tray_enabled,
                        )),
                )
                .child(caption(
                    "System tray changes require restarting the graphical host.",
                )),
        );

        column()
            .gap_3()
            .child(library)
            .child(playback)
            .child(audio)
            .child(interface)
            .child(desktop)
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
                "visual-waveform",
                "Waveform",
                selected == VisualPage::Waveform,
                cx,
                |settings| settings.visual_page = VisualPage::Waveform,
            ))
            .child(self.settings.tab_button(
                "visual-audio-visualizer",
                "Radial Spectrum",
                selected == VisualPage::AudioVisualizer,
                cx,
                |settings| settings.visual_page = VisualPage::AudioVisualizer,
            ))
    }

    fn visual_settings(&mut self, cx: &mut Context<GuiApp>) -> Div {
        let settings = &self.settings;
        let draft = &settings.draft;
        match settings.visual_page {
            VisualPage::Common => {
                let palette = dropdown_button("visual-palette", visualization_palette_label(draft.visual_palette), cx, |this, _, _| {
                    let values = [VisualizationPalette::TokyoNight, VisualizationPalette::Deadbeef];
                    let selected = values.iter().position(|value| *value == this.settings.draft.visual_palette).unwrap_or(0);
                    this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::Palette(value), visualization_palette_label(value))).collect(), selected);
                }).relative().child(self.measurement(Measured::SettingsStyle));
                let fft = dropdown_button("spectrum-fft", draft.spectrum_fft_size.to_string(), cx, |this, _, _| {
                    let values = [512, 1024, 2048, 4096, 8192, 16384, 32768];
                    let selected = values.iter().position(|value| *value == this.settings.draft.spectrum_fft_size).unwrap_or(0);
                    this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::Fft(value), value.to_string())).collect(), selected);
                }).relative().child(self.measurement(Measured::SettingsFft));
                let window = dropdown_button("spectrum-window", SettingChoice::Window(draft.spectrum_window).label(), cx, |this, _, _| {
                    let values = [SpectrumWindow::Hann, SpectrumWindow::BlackmanHarris, SpectrumWindow::None];
                    let selected = values.iter().position(|value| *value == this.settings.draft.spectrum_window).unwrap_or(0);
                    this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::Window(value), SettingChoice::Window(value).label())).collect(), selected);
                }).relative().child(self.measurement(Measured::SettingsWindow));
                column().gap_3()
                    .child(settings_group("Appearance", column().gap_2()
                        .child(two_column(settings.field(Field::Background).min_w(px(180.)), form_field("Color scheme", palette).min_w(px(180.))))
                        .child(caption("Background: all views. Color scheme: Spectrum, Spectrogram, and Waveform."))))
                    .child(settings_group("Frequency analysis", column().gap_2()
                        .child(two_column(form_field("FFT size", fft).min_w(px(180.)), form_field("FFT window", window).min_w(px(180.))))
                        .child(settings.pair(Field::Fps, Field::SpectrumDb))
                        .child(caption("FFT and rate: Spectrum, Spectrogram, and Radial Spectrum. FFT size or window changes clear Spectrogram history."))
                        .child(caption("The dB range excludes Spectrogram, which has its own color range."))))
                    .child(settings_group("Rendering", column().gap_2()
                        .child(row().child(visual_switch("visual-cache", "▤", "Reuse rendering caches", draft.visualization_cache, cx, |draft| draft.visualization_cache = !draft.visualization_cache)))
                        .child(caption("Reuse graphics in Spectrum, Spectrogram, and Waveform: less CPU work, more memory."))))
            }
            VisualPage::Spectrum => {
                let solid = draft.spectrum_style == SpectrumStyle::Solid;
                let style = dropdown_button("spectrum-style", spectrum_style_label(draft.spectrum_style), cx, |this, _, _| {
                    let values = [SpectrumStyle::Bars, SpectrumStyle::Outline, SpectrumStyle::Led, SpectrumStyle::Line, SpectrumStyle::Solid];
                    let selected = values.iter().position(|value| *value == this.settings.draft.spectrum_style).unwrap_or(0);
                    this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::SpectrumStyle(value), spectrum_style_label(value))).collect(), selected);
                }).relative().child(self.measurement(Measured::SettingsStyle));
                column().gap_3()
                    .child(settings_group("Shape & detail", column().gap_2()
                        .child(two_column(form_field("Style", style).min_w(px(180.)), settings.field_state(Field::SpectrumBars, !solid, "Solid uses pixel density, not a requested bar count", cx)))
                        .child(two_column(
                            settings.field_state(Field::SpectrumBarWidth, !solid, "Solid uses pixel density, not auto bar width", cx),
                            settings.field_state(Field::SpectrumGap, !solid, "Solid has no bar gaps", cx),
                        ))
                        .child(row().flex_wrap().gap_2()
                            .child(visual_switch("spectrum-interpolate", "≈", "Frequency interpolation", draft.spectrum_interpolate, cx, |draft| draft.spectrum_interpolate = !draft.spectrum_interpolate))
                            .child(visual_switch("spectrum-grid", "#", "Faint grid", draft.spectrum_grid, cx, |draft| draft.spectrum_grid = !draft.spectrum_grid))
                            .child(visual_switch("spectrum-labels", "T", "Labels", draft.spectrum_labels, cx, |draft| draft.spectrum_labels = !draft.spectrum_labels)))
                        .child(caption("Auto count uses width + gap. Solid uses pixel density. FFT and dB range: Common."))))
                    .child(settings_group("Body motion", column().gap_2()
                        .child(settings.pair(Field::SpectrumBarHold, Field::SpectrumBarGravity))
                        .child(settings.single(Field::SpectrumSmoothing))
                        .child(caption("Gravity 0 snaps after hold, bypassing smoothing. Attacks stay immediate."))))
                    .child(settings_group("Peak markers", column().gap_2()
                        .child(row().child(visual_switch("spectrum-peaks", "∧", "Show peaks", draft.spectrum_peaks, cx, |draft| draft.spectrum_peaks = !draft.spectrum_peaks)))
                        .child(two_column(
                            settings.field_state(Field::SpectrumHold, draft.spectrum_peaks, "Enable peak markers to edit peak hold", cx),
                            settings.field_state(Field::SpectrumGravity, draft.spectrum_peaks, "Enable peak markers to edit peak gravity", cx),
                        ))))
            }
            VisualPage::Spectrogram => column().gap_3()
                .child(settings_group("Color & history", column().gap_2()
                    .child(settings.pair(Field::SpectrogramDb, Field::SpectrogramHistory))
                    .child(row().child(visual_switch("spectrogram-labels", "T", "Labels", draft.spectrogram_labels, cx, |draft| draft.spectrogram_labels = !draft.spectrogram_labels)))
                    .child(caption("History is limited by width and analysis rate. Shared analysis and colors: Common."))))
                .child(settings_group("Frequency sampling", column().gap_2()
                    .child(settings.pair(Field::SpectrogramSamplingPointsScale, Field::SpectrogramInterpolationPoints))
                    .child(row().child(visual_switch("spectrogram-interpolate", "≈", "Frequency interpolation", draft.spectrogram_interpolate, cx, |draft| draft.spectrogram_interpolate = !draft.spectrogram_interpolate)))
                    .child(caption("Row limit applies with or without interpolation; it does not add FFT detail. Source rows follow the tallest visible panel.")))),
            VisualPage::AudioVisualizer => {
                let bars = draft.radial_spectrum_style != RadialSpectrumStyle::Rings;
                let rings = draft.radial_spectrum_style != RadialSpectrumStyle::Bars;
                let style = dropdown_button("audio-visualizer-style", radial_spectrum_style_label(draft.radial_spectrum_style), cx, |this, _, _| {
                    let values = [RadialSpectrumStyle::Bars, RadialSpectrumStyle::Rings, RadialSpectrumStyle::BarsRings];
                    let selected = values.iter().position(|value| *value == this.settings.draft.radial_spectrum_style).unwrap_or(0);
                    this.open_setting_dropdown(values.into_iter().map(|value| DropdownItem::new(SettingChoice::RadialSpectrumStyle(value), radial_spectrum_style_label(value))).collect(), selected);
                }).relative().child(self.measurement(Measured::SettingsStyle));
                column().gap_3()
                    .child(settings_group("Shape & motion", column().gap_2()
                        .child(two_column(form_field("Style", style).min_w(px(180.)), settings.field(Field::RadialSpectrumInnerDiameter).min_w(px(180.))))
                        .child(settings.pair(Field::RadialSpectrumSensitivity, Field::RadialSpectrumRotationSpeed))
                        .child(row().child(visual_switch("audio-visualizer-fade-idle", "◌", "Fade when not playing", draft.radial_spectrum_fade_when_idle, cx, |draft| draft.radial_spectrum_fade_when_idle = !draft.radial_spectrum_fade_when_idle)))
                        .child(caption("Noctalia Fancy. Shared analysis and background: Common. Inner diameter is relative to panel size."))))
                    .child(settings_group("Bars & rings", column().gap_2()
                        .child(two_column(
                            settings.field_state(Field::RadialSpectrumBarWidth, bars, "Select Bars or Bars + Rings to edit bar width", cx),
                            settings.field_state(Field::RadialSpectrumBarGlowLayers, bars, "Select Bars or Bars + Rings to edit bar glow", cx),
                        ))
                        .child(two_column(
                            settings.field_state(Field::RadialSpectrumRingOpacity, rings, "Select Rings or Bars + Rings to edit ring opacity", cx),
                            settings.field(Field::RadialSpectrumBloomIntensity).min_w(px(180.)),
                        ))
                        .child(caption("Bloom affects bars and rings; bar glow requires bloom above zero."))))
                    .child(settings_group("Colors", column().gap_2()
                        .child(settings.pair(Field::RadialSpectrumPrimaryColor, Field::RadialSpectrumSecondaryColor))
                        .child(caption("Independent of the shared color scheme."))))
            }
            VisualPage::Waveform => column().gap_3()
                .child(settings_group("Appearance", column().gap_2()
                    .child(settings.pair(Field::CursorColor, Field::Glow))
                    .child(row().child(visual_switch("waveform-labels", "T", "Labels", draft.waveform_labels, cx, |draft| draft.waveform_labels = !draft.waveform_labels)))
                    .child(caption("Shared appearance: Common. The waterline stays still while paused."))))
                .child(settings_group("Amplitude display", column().gap_2()
                    .child(settings.pair(Field::WaveformRmsGain, Field::WaveformPeakGain))
                    .child(settings.single(Field::WaveformPeakGamma))
                    .child(caption("Display only, not playback volume. Gamma > 1 reduces low peaks; < 1 lifts them.")))),
        }
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
            (_, VisualPage::AudioVisualizer) => "settings-audio-visualizer-scroll",
        };
        let content = match self.settings.page {
            SettingsPage::General => self.general_settings(cx),
            SettingsPage::Visualizations => {
                panel = panel.child(self.visual_settings_tabs(cx));
                self.visual_settings(cx)
            }
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
            status_message("Applying settings…", StatusKind::Info).into_any_element()
        } else {
            match &self.settings.feedback {
                Some(Err(error)) => copyable_message("copy-settings-error", error.clone(), cx)
                    .text_color(rgb(ERROR))
                    .into_any_element(),
                Some(Ok(())) => {
                    status_message("Last apply succeeded", StatusKind::Success).into_any_element()
                }
                None => div().into_any_element(),
            }
        };
        panel = panel.child(
            row()
                .gap_3()
                .flex_shrink_0()
                .px(gpui::px(UI_INSET))
                .py(gpui::px(UI_INSET))
                .min_h(rems(3.))
                .border_t_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_sm()
                        .child(feedback),
                )
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
                let row_height = DROPDOWN_ROW_REMS * f32::from(window.rem_size());
                let height = (item_count as f32 * row_height)
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
                                let label: SharedString = item.label.clone().into();
                                let hint = label.clone();
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
